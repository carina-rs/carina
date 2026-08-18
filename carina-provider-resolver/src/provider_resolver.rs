//! Provider resolution: download, extract, cache, and verify provider binaries.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use carina_core::parser::ProviderConfig;

use crate::signing::{self, ExpectedIdentity};

/// Distinguishes the three shapes a lock entry can take. Encoded as a tagged
/// enum so that invalid field combinations (e.g. `version = ""` *and*
/// `revision = "main"`, the root cause of #2028) can't be constructed at
/// all — no runtime validator, no empty-string filler.
///
/// Serialized with an explicit `mode` discriminator so the on-disk shape is
/// unambiguous:
///
/// ```toml
/// [[provider]]
/// name = "aws"; source = "..."; sha256 = "..."
/// mode = "version"
/// version = "0.5.2"
/// constraint = "~0.5.0"
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "mode", rename_all = "lowercase")]
pub enum LockEntryKind {
    /// Released provider pinned to a semver tag (or registry version).
    Version {
        version: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        constraint: Option<String>,
    },
    /// Provider built from a git revision (branch/tag/SHA) via CI artifacts.
    Revision {
        revision: String,
        resolved_sha: String,
    },
    /// Registry-hosted provider resolved by following a branch revision.
    RegistryRevision { revision: String, version: String },
    /// Local `file://` provider — identified entirely by `source`.
    File,
}

impl LockEntryKind {
    /// The concrete published version this entry pins, for modes that have one.
    fn resolved_version(&self) -> Option<&str> {
        match self {
            LockEntryKind::Version { version, .. }
            | LockEntryKind::RegistryRevision { version, .. } => Some(version),
            LockEntryKind::Revision { .. } | LockEntryKind::File => None,
        }
    }
}

/// Uninhabited marker for an entry that cannot carry registry metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum NoRegistryLock {}

/// A single provider entry in carina-providers.lock.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(bound(serialize = "R: Serialize", deserialize = "R: Deserialize<'de>"))]
pub struct LockEntry<R = NoRegistryLock> {
    pub name: String,
    pub source: String,
    #[serde(flatten)]
    pub kind: LockEntryKind,
    pub sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registry: Option<R>,
}

impl LockEntry<NoRegistryLock> {
    fn into_stored(self) -> LockEntry<RegistryLock> {
        let registry = match self.registry {
            None => None,
            Some(uninhabited) => match uninhabited {},
        };
        LockEntry {
            name: self.name,
            source: self.source,
            kind: self.kind,
            sha256: self.sha256,
            registry,
        }
    }
}

/// Registry-only pinning metadata. Kept outside [`LockEntryKind`] so the
/// version/revision/file shape remains a closed tagged enum.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(into = "RegistryLockSerde")]
pub struct RegistryLock {
    resolved_hostname: String,
    /// The greatest fully validated sequence observed for this source. This is
    /// durable across downstream failures but is not a rollback floor.
    sequence: RegistrySequence,
    sequence_anchor: RegistrySequenceAnchor,
    valid_until_present: bool,
    yanked_versions: YankedRegistryVersions,
    signature: RegistrySignatureProtection,
    transparency_log_present: bool,
}

const PROVIDERS_V1_DISCOVERY_FIELD: &str = "providers.v1";
const REGISTRY_DISCOVERY_PATH: &str = "/.well-known/carina.json";

mod unconsumed_discovery_values {
    use super::{BTreeMap, PROVIDERS_V1_DISCOVERY_FIELD, fmt};

    /// Discovery values retained for round-tripping but not consumed by this
    /// client. The map is deliberately opaque: adding a consumer requires an
    /// explicit API change here instead of an unnoticed lookup in retained
    /// material.
    #[derive(Debug, Clone, Default, PartialEq, Eq)]
    pub(super) struct UnconsumedDiscoveryValues(BTreeMap<String, String>);

    #[derive(Debug)]
    pub(super) struct UnconsumedDiscoveryValuesError;

    impl fmt::Display for UnconsumedDiscoveryValuesError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(
                f,
                "unconsumed registry discovery values must not contain providers.v1"
            )
        }
    }

    impl std::error::Error for UnconsumedDiscoveryValuesError {}

    impl UnconsumedDiscoveryValues {
        pub(super) fn try_from_values(
            values: BTreeMap<String, String>,
        ) -> Result<Self, UnconsumedDiscoveryValuesError> {
            if values.contains_key(PROVIDERS_V1_DISCOVERY_FIELD) {
                return Err(UnconsumedDiscoveryValuesError);
            }
            Ok(Self(values))
        }

        pub(super) fn without_consumed(mut values: BTreeMap<String, String>) -> Self {
            values.remove(PROVIDERS_V1_DISCOVERY_FIELD);
            Self(values)
        }

        pub(super) fn extend_resolved_values<'a>(
            &'a self,
            resolved: &mut BTreeMap<&'a str, &'a str>,
        ) {
            resolved.extend(
                self.0
                    .iter()
                    .map(|(field, value)| (field.as_str(), value.as_str())),
            );
        }

        pub(super) fn into_values(self) -> BTreeMap<String, String> {
            self.0
        }

        pub(super) fn is_empty(&self) -> bool {
            self.0.is_empty()
        }
    }
}

use unconsumed_discovery_values::{UnconsumedDiscoveryValues, UnconsumedDiscoveryValuesError};

/// The indivisible resolved discovery values pinned for one registry host.
///
/// The open map is intentional: values written by a newer client remain
/// representable and survive lock updates by an older client that does not
/// consume them yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryDiscoveryPin {
    api_base_url: String,
    additional: UnconsumedDiscoveryValues,
}

#[derive(Debug)]
enum RegistryDiscoveryPinError {
    MissingProvidersV1,
    InvalidApiBaseUrl {
        value: String,
        source: Option<url::ParseError>,
    },
}

impl fmt::Display for RegistryDiscoveryPinError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingProvidersV1 => write!(
                f,
                "registry discovery values are missing required providers.v1"
            ),
            Self::InvalidApiBaseUrl { value, source } => {
                write!(
                    f,
                    "registry discovery providers.v1 pinned value must be an absolute HTTPS URL: {value:?}"
                )?;
                if let Some(source) = source {
                    write!(f, ": {source}")?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for RegistryDiscoveryPinError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::InvalidApiBaseUrl {
                source: Some(source),
                ..
            } => Some(source),
            Self::MissingProvidersV1 | Self::InvalidApiBaseUrl { source: None, .. } => None,
        }
    }
}

impl RegistryDiscoveryPin {
    fn from_values(values: BTreeMap<String, String>) -> Result<Self, RegistryDiscoveryPinError> {
        let api_base_url = values
            .get(PROVIDERS_V1_DISCOVERY_FIELD)
            .cloned()
            .ok_or(RegistryDiscoveryPinError::MissingProvidersV1)?;
        validate_persisted_api_base_url(&api_base_url)?;
        Ok(Self {
            api_base_url,
            additional: UnconsumedDiscoveryValues::without_consumed(values),
        })
    }

    fn into_values(self) -> BTreeMap<String, String> {
        let mut values = self.additional.into_values();
        values.insert(PROVIDERS_V1_DISCOVERY_FIELD.into(), self.api_base_url);
        values
    }

    fn resolved_values(&self) -> BTreeMap<&str, &str> {
        let mut values = BTreeMap::new();
        self.additional.extend_resolved_values(&mut values);
        values.insert(PROVIDERS_V1_DISCOVERY_FIELD, &self.api_base_url);
        values
    }

    fn api_base_url(&self) -> &str {
        &self.api_base_url
    }

    fn consumed_values(&self) -> Self {
        Self {
            api_base_url: self.api_base_url.clone(),
            additional: UnconsumedDiscoveryValues::default(),
        }
    }

    /// Iterate over the concrete discovery values held by this pin.
    pub fn values(&self) -> impl Iterator<Item = (&str, &str)> {
        self.resolved_values().into_iter()
    }
}

impl Serialize for RegistryDiscoveryPin {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.resolved_values().serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for RegistryDiscoveryPin {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let values = BTreeMap::<String, String>::deserialize(deserializer)?;
        Self::from_values(values).map_err(serde::de::Error::custom)
    }
}

/// A host either has its currently consumed discovery values pinned or is
/// explicitly awaiting first contact for them after an operator-authorized
/// re-pin. Opaque values retained from newer clients remain carried in either
/// state.
#[derive(Debug, Clone, PartialEq, Eq)]
enum RegistryDiscoveryPinState {
    Pinned(RegistryDiscoveryPin),
    Unpinned(UnconsumedDiscoveryValues),
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(into = "RegistryHostLockSerde")]
struct RegistryHostLock {
    discovery: RegistryDiscoveryPinState,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RegistryHostLockSerde {
    discovery_pin_present: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    discovery_values: Option<BTreeMap<String, String>>,
}

#[derive(Debug)]
enum RegistryHostLockError {
    Inconsistent,
    InvalidDiscoveryPin(RegistryDiscoveryPinError),
    InvalidUnconsumedDiscoveryValues(UnconsumedDiscoveryValuesError),
}

impl fmt::Display for RegistryHostLockError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Inconsistent => write!(
                f,
                "registry host lock is inconsistent: providers.v1 must be present in discovery_values exactly when discovery_pin_present is true; other discovery values may remain while it is false"
            ),
            Self::InvalidDiscoveryPin(source) => source.fmt(f),
            Self::InvalidUnconsumedDiscoveryValues(source) => source.fmt(f),
        }
    }
}

impl std::error::Error for RegistryHostLockError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::InvalidDiscoveryPin(source) => Some(source),
            Self::InvalidUnconsumedDiscoveryValues(source) => Some(source),
            Self::Inconsistent => None,
        }
    }
}

impl RegistryHostLock {
    fn pinned(pin: RegistryDiscoveryPin) -> Self {
        Self {
            discovery: RegistryDiscoveryPinState::Pinned(pin),
        }
    }

    fn pin(&self) -> Option<&RegistryDiscoveryPin> {
        match &self.discovery {
            RegistryDiscoveryPinState::Pinned(pin) => Some(pin),
            RegistryDiscoveryPinState::Unpinned(_) => None,
        }
    }

    fn additional_discovery_values(&self) -> &UnconsumedDiscoveryValues {
        match &self.discovery {
            RegistryDiscoveryPinState::Pinned(pin) => &pin.additional,
            RegistryDiscoveryPinState::Unpinned(additional) => additional,
        }
    }
}

impl TryFrom<RegistryHostLockSerde> for RegistryHostLock {
    type Error = RegistryHostLockError;

    fn try_from(value: RegistryHostLockSerde) -> Result<Self, Self::Error> {
        let discovery = match (value.discovery_pin_present, value.discovery_values) {
            (true, Some(values)) => RegistryDiscoveryPinState::Pinned(
                RegistryDiscoveryPin::from_values(values)
                    .map_err(RegistryHostLockError::InvalidDiscoveryPin)?,
            ),
            (false, None) => {
                RegistryDiscoveryPinState::Unpinned(UnconsumedDiscoveryValues::default())
            }
            (false, Some(additional)) => RegistryDiscoveryPinState::Unpinned(
                UnconsumedDiscoveryValues::try_from_values(additional)
                    .map_err(RegistryHostLockError::InvalidUnconsumedDiscoveryValues)?,
            ),
            _ => return Err(RegistryHostLockError::Inconsistent),
        };
        Ok(Self { discovery })
    }
}

impl From<RegistryHostLock> for RegistryHostLockSerde {
    fn from(value: RegistryHostLock) -> Self {
        match value.discovery {
            RegistryDiscoveryPinState::Pinned(pin) => Self {
                discovery_pin_present: true,
                discovery_values: Some(pin.into_values()),
            },
            RegistryDiscoveryPinState::Unpinned(additional) => Self {
                discovery_pin_present: false,
                discovery_values: if additional.is_empty() {
                    None
                } else {
                    Some(additional.into_values())
                },
            },
        }
    }
}

impl<'de> Deserialize<'de> for RegistryHostLock {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        RegistryHostLockSerde::deserialize(deserializer)?
            .try_into()
            .map_err(serde::de::Error::custom)
    }
}

/// Versions that this lock has observed as yanked. The resolver can only add
/// observations, so normal lock updates cannot remove a recorded yank.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(transparent)]
pub struct YankedRegistryVersions(BTreeSet<String>);

impl YankedRegistryVersions {
    pub fn contains(&self, version: &str) -> bool {
        self.0.contains(version)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    fn from_serialized(versions: BTreeSet<String>) -> Self {
        Self(versions)
    }

    fn into_serialized(self) -> BTreeSet<String> {
        self.0
    }

    fn with_observed(mut self, versions: &[RegistryVersion]) -> Self {
        self.0.extend(
            versions
                .iter()
                .filter(|version| version.yanked)
                .map(|version| version.version.clone()),
        );
        self
    }

    fn union(mut self, other: &Self) -> Self {
        self.0.extend(other.0.iter().cloned());
        self
    }

    fn stripped_from(&self, versions: &[RegistryVersion]) -> Vec<String> {
        self.0
            .iter()
            .filter(|known_yanked| {
                let is_present = versions
                    .iter()
                    .any(|version| version.version == known_yanked.as_str());
                let is_still_yanked = versions
                    .iter()
                    .any(|version| version.version == known_yanked.as_str() && version.yanked);
                is_present && !is_still_yanked
            })
            .cloned()
            .collect()
    }
}

/// Security observations made before a provider has a registry lock entry to
/// carry them. An entry moves out of this map as soon as the provider is
/// pinned, keeping each source's ratchets in exactly one persisted location.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(transparent)]
struct UnpinnedRegistryRatchets(BTreeMap<String, RegistryRatchets>);

impl UnpinnedRegistryRatchets {
    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    fn get(&self, source: &str) -> Option<&RegistryRatchets> {
        self.0.get(source)
    }

    fn merge(
        &mut self,
        source: String,
        ratchets: RegistryRatchets,
    ) -> Result<(), RegistryIdentityPinConflict> {
        match self.0.entry(source) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(ratchets);
            }
            std::collections::btree_map::Entry::Occupied(mut entry) => {
                let merged = entry.get().clone().merge(&ratchets)?;
                entry.insert(merged);
            }
        }
        Ok(())
    }

    fn remove(&mut self, source: &str) -> Option<RegistryRatchets> {
        self.0.remove(source)
    }

    fn into_canonical(self) -> Result<Self, RegistryIdentityPinConflict> {
        let mut canonical = Self::default();
        for (source, ratchets) in self.0 {
            canonical.merge(canonical_lock_source(&source), ratchets)?;
        }
        Ok(canonical)
    }
}

/// Whether a fully validated registry listing carried a sequence observation.
/// The contradictory `present = true, value = None` state cannot be constructed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistrySequence {
    Absent,
    Present(u64),
}

/// The anti-rollback sequence established by a successful provider resolve.
/// An unpinned observation has no numeric value that can accidentally be used
/// as either a rollback floor or a fast-forward baseline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RegistrySequenceAnchor {
    Unestablished,
    Established(u64),
}

/// Signature protection recorded for a registry provider. The signature
/// requirement is a monotonic ratchet, while the identity pin can be cleared
/// explicitly so the next signed artifact establishes a replacement pin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistrySignatureProtection {
    NotRequired,
    RequiredUnpinned,
    RequiredPinned(IdentityPin),
}

/// A signing-identity pin whose identity and issuer are always present together.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentityPin {
    pub certificate_identity: String,
    pub certificate_oidc_issuer: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryIdentityPinConflict {
    pub left: IdentityPin,
    pub right: IdentityPin,
}

impl fmt::Display for RegistryIdentityPinConflict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "conflicting registry identity pins: {} (issuer {}) and {} (issuer {})",
            self.left.certificate_identity,
            self.left.certificate_oidc_issuer,
            self.right.certificate_identity,
            self.right.certificate_oidc_issuer
        )
    }
}

impl std::error::Error for RegistryIdentityPinConflict {}

/// Freshness values discarded by an explicit registry re-bootstrap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RegistryFreshness {
    pub sequence: Option<u64>,
    pub sequence_anchor: Option<u64>,
}

enum RegistryRecoveryTarget<R> {
    Pinned {
        index: usize,
        state: R,
        sequence_anchor: RegistrySequenceAnchor,
    },
    Unpinned {
        state: R,
    },
}

impl<R> RegistryRecoveryTarget<R> {
    fn state(&self) -> &R {
        match self {
            Self::Pinned { state, .. } | Self::Unpinned { state } => state,
        }
    }

    fn sequence_anchor(&self) -> RegistrySequenceAnchor {
        match self {
            Self::Pinned {
                sequence_anchor, ..
            } => *sequence_anchor,
            Self::Unpinned { .. } => RegistrySequenceAnchor::Unestablished,
        }
    }

    fn map_state<S>(self, map: impl FnOnce(R) -> S) -> RegistryRecoveryTarget<S> {
        match self {
            Self::Pinned {
                index,
                state,
                sequence_anchor,
            } => RegistryRecoveryTarget::Pinned {
                index,
                state: map(state),
                sequence_anchor,
            },
            Self::Unpinned { state } => RegistryRecoveryTarget::Unpinned { state: map(state) },
        }
    }

    fn try_map_state<S, E>(
        self,
        map: impl FnOnce(R) -> Result<S, E>,
    ) -> Result<RegistryRecoveryTarget<S>, E> {
        match self {
            Self::Pinned {
                index,
                state,
                sequence_anchor,
            } => Ok(RegistryRecoveryTarget::Pinned {
                index,
                state: map(state)?,
                sequence_anchor,
            }),
            Self::Unpinned { state } => Ok(RegistryRecoveryTarget::Unpinned { state: map(state)? }),
        }
    }
}

/// Failure to inspect or mutate registry provider or host recovery state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistryLockRecoveryError {
    InvalidProvider { provider: String, reason: String },
    NotRegistryProvider { provider: String },
    ProviderStateNotFound { provider: String },
    RegistryHostStateNotFound { host: String },
    DiscoveryAlreadyUnpinned { host: String },
    SignatureNotRequired { provider: String },
    IdentityAlreadyUnpinned { provider: String },
    IdentityPinConflict(RegistryIdentityPinConflict),
}

impl fmt::Display for RegistryLockRecoveryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidProvider { provider, reason } => {
                write!(f, "Invalid registry provider {provider:?}: {reason}")
            }
            Self::NotRegistryProvider { provider } => write!(
                f,
                "Provider {provider:?} is not registry-hosted; provider recovery operations apply only to [hostname/]namespace/name sources"
            ),
            Self::ProviderStateNotFound { provider } => write!(
                f,
                "No registry security state for provider {provider:?} was found in carina-providers.lock"
            ),
            Self::RegistryHostStateNotFound { host } => write!(
                f,
                "No registry discovery state for host {host:?} was found in carina-providers.lock"
            ),
            Self::DiscoveryAlreadyUnpinned { host } => write!(
                f,
                "Registry host {host:?} is already awaiting a new discovery pin"
            ),
            Self::SignatureNotRequired { provider } => write!(
                f,
                "Registry provider {provider:?} has no ratcheted signature requirement or identity pin to replace"
            ),
            Self::IdentityAlreadyUnpinned { provider } => write!(
                f,
                "Registry provider {provider:?} already requires a signature and is awaiting a new identity pin"
            ),
            Self::IdentityPinConflict(source) => source.fmt(f),
        }
    }
}

impl std::error::Error for RegistryLockRecoveryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::IdentityPinConflict(source) => Some(source),
            Self::InvalidProvider { .. }
            | Self::NotRegistryProvider { .. }
            | Self::ProviderStateNotFound { .. }
            | Self::RegistryHostStateNotFound { .. }
            | Self::DiscoveryAlreadyUnpinned { .. }
            | Self::SignatureNotRequired { .. }
            | Self::IdentityAlreadyUnpinned { .. } => None,
        }
    }
}

/// Durable security observations for one registry source. This is the shared
/// representation used both by pinned provider entries and by observations
/// that must survive before the provider itself can be pinned.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(into = "RegistryRatchetsSerde")]
struct RegistryRatchets {
    sequence: RegistrySequence,
    valid_until_present: bool,
    yanked_versions: YankedRegistryVersions,
    signature: RegistrySignatureProtection,
    transparency_log_present: bool,
}

struct ResolvedRegistryRecovery {
    source_key: String,
    target: RegistryRecoveryTarget<RegistryRatchets>,
}

struct RegistryIdentityRepinState {
    pin: IdentityPin,
    residual: RegistryIdentityRepinResidual,
}

struct RegistryIdentityRepinResidual {
    sequence: RegistrySequence,
    valid_until_present: bool,
    yanked_versions: YankedRegistryVersions,
    transparency_log_present: bool,
}

struct RegistryRebootstrapState {
    sequence: RegistrySequence,
    residual: RegistryRebootstrapResidual,
}

struct RegistryRebootstrapResidual {
    valid_until_present: bool,
    yanked_versions: YankedRegistryVersions,
    signature: RegistrySignatureProtection,
    transparency_log_present: bool,
}

impl Default for RegistryRatchets {
    fn default() -> Self {
        Self {
            sequence: RegistrySequence::Absent,
            valid_until_present: false,
            yanked_versions: YankedRegistryVersions::default(),
            signature: RegistrySignatureProtection::NotRequired,
            transparency_log_present: false,
        }
    }
}

impl RegistryRatchets {
    fn merge(mut self, other: &Self) -> Result<Self, RegistryIdentityPinConflict> {
        let signature_required = self.signature.is_required() || other.signature.is_required();
        let identity_pin = match (
            self.signature.identity_pin(),
            other.signature.identity_pin(),
        ) {
            (Some(left), Some(right)) if left == right => Some(left.clone()),
            (Some(left), Some(right)) => {
                return Err(RegistryIdentityPinConflict {
                    left: left.clone(),
                    right: right.clone(),
                });
            }
            (Some(pin), None) | (None, Some(pin)) => Some(pin.clone()),
            (None, None) => None,
        };
        let signature = match (signature_required, identity_pin) {
            (_, Some(pin)) => RegistrySignatureProtection::RequiredPinned(pin),
            (true, None) => RegistrySignatureProtection::RequiredUnpinned,
            (false, None) => RegistrySignatureProtection::NotRequired,
        };
        self.sequence = match (self.sequence.value(), other.sequence.value()) {
            (Some(left), Some(right)) => RegistrySequence::Present(left.max(right)),
            (None, Some(sequence)) => RegistrySequence::Present(sequence),
            (Some(sequence), None) => RegistrySequence::Present(sequence),
            (None, None) => RegistrySequence::Absent,
        };
        self.valid_until_present |= other.valid_until_present;
        self.yanked_versions = self.yanked_versions.union(&other.yanked_versions);
        self.signature = signature;
        self.transparency_log_present |= other.transparency_log_present;
        Ok(self)
    }

    fn merge_into_registry(
        self,
        registry: &mut RegistryLock,
    ) -> Result<(), RegistryIdentityPinConflict> {
        let merged = RegistryRatchets::from(&*registry).merge(&self)?;
        let Self {
            sequence,
            valid_until_present,
            yanked_versions,
            signature,
            transparency_log_present,
        } = merged;
        registry.sequence = sequence;
        registry.valid_until_present = valid_until_present;
        registry.yanked_versions = yanked_versions;
        registry.signature = signature;
        registry.transparency_log_present = transparency_log_present;
        Ok(())
    }

    fn into_identity_repin_state(
        self,
        provider: String,
    ) -> Result<RegistryIdentityRepinState, RegistryLockRecoveryError> {
        let Self {
            sequence,
            valid_until_present,
            yanked_versions,
            signature,
            transparency_log_present,
        } = self;
        let pin = match signature {
            RegistrySignatureProtection::RequiredPinned(pin) => pin,
            RegistrySignatureProtection::RequiredUnpinned => {
                return Err(RegistryLockRecoveryError::IdentityAlreadyUnpinned { provider });
            }
            RegistrySignatureProtection::NotRequired => {
                return Err(RegistryLockRecoveryError::SignatureNotRequired { provider });
            }
        };
        Ok(RegistryIdentityRepinState {
            pin,
            residual: RegistryIdentityRepinResidual {
                sequence,
                valid_until_present,
                yanked_versions,
                transparency_log_present,
            },
        })
    }

    fn into_rebootstrap_state(self) -> RegistryRebootstrapState {
        let Self {
            sequence,
            valid_until_present,
            yanked_versions,
            signature,
            transparency_log_present,
        } = self;
        RegistryRebootstrapState {
            sequence,
            residual: RegistryRebootstrapResidual {
                valid_until_present,
                yanked_versions,
                signature,
                transparency_log_present,
            },
        }
    }
}

impl RegistryIdentityRepinResidual {
    fn into_ratchets(self) -> RegistryRatchets {
        RegistryRatchets {
            sequence: self.sequence,
            valid_until_present: self.valid_until_present,
            yanked_versions: self.yanked_versions,
            signature: RegistrySignatureProtection::RequiredUnpinned,
            transparency_log_present: self.transparency_log_present,
        }
    }

    fn apply_to_registry(self, registry: &mut RegistryLock) {
        let RegistryRatchets {
            sequence,
            valid_until_present,
            yanked_versions,
            signature,
            transparency_log_present,
        } = self.into_ratchets();
        registry.sequence = sequence;
        registry.valid_until_present = valid_until_present;
        registry.yanked_versions = yanked_versions;
        registry.signature = signature;
        registry.transparency_log_present = transparency_log_present;
    }
}

impl RegistryRebootstrapResidual {
    fn into_ratchets(self) -> RegistryRatchets {
        RegistryRatchets {
            sequence: RegistrySequence::Absent,
            valid_until_present: self.valid_until_present,
            yanked_versions: self.yanked_versions,
            signature: self.signature,
            transparency_log_present: self.transparency_log_present,
        }
    }

    fn apply_to_registry(self, registry: &mut RegistryLock) {
        let RegistryRatchets {
            sequence,
            valid_until_present,
            yanked_versions,
            signature,
            transparency_log_present,
        } = self.into_ratchets();
        registry.sequence = sequence;
        registry.valid_until_present = valid_until_present;
        registry.yanked_versions = yanked_versions;
        registry.signature = signature;
        registry.transparency_log_present = transparency_log_present;
    }
}

impl UnpinnedRegistryRatchets {
    fn commit_identity_repin(
        &mut self,
        source: &str,
        residual: RegistryIdentityRepinResidual,
    ) -> bool {
        let Some(ratchets) = self.0.get_mut(source) else {
            return false;
        };
        *ratchets = residual.into_ratchets();
        true
    }

    fn commit_rebootstrap(&mut self, source: &str, residual: RegistryRebootstrapResidual) -> bool {
        let Some(ratchets) = self.0.get_mut(source) else {
            return false;
        };
        *ratchets = residual.into_ratchets();
        true
    }
}

impl From<&RegistryLock> for RegistryRatchets {
    fn from(registry: &RegistryLock) -> Self {
        Self {
            sequence: registry.sequence.clone(),
            valid_until_present: registry.valid_until_present,
            yanked_versions: registry.yanked_versions.clone(),
            signature: registry.signature.clone(),
            transparency_log_present: registry.transparency_log_present,
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RegistryRatchetsSerde {
    sequence_present: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sequence: Option<u64>,
    valid_until_present: bool,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    yanked_versions: BTreeSet<String>,
    signature_present: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    certificate_identity: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    certificate_oidc_issuer: Option<String>,
    transparency_log_present: bool,
}

impl TryFrom<RegistryRatchetsSerde> for RegistryRatchets {
    type Error = RegistryLockError;

    fn try_from(value: RegistryRatchetsSerde) -> Result<Self, Self::Error> {
        let sequence = match (value.sequence_present, value.sequence) {
            (false, None) => RegistrySequence::Absent,
            (true, Some(sequence)) => RegistrySequence::Present(sequence),
            _ => return Err(RegistryLockError::InconsistentSequence),
        };
        let signature = RegistrySignatureProtection::from_serialized(
            value.signature_present,
            value.certificate_identity,
            value.certificate_oidc_issuer,
        )?;
        Ok(Self {
            sequence,
            valid_until_present: value.valid_until_present,
            yanked_versions: YankedRegistryVersions::from_serialized(value.yanked_versions),
            signature,
            transparency_log_present: value.transparency_log_present,
        })
    }
}

impl From<RegistryRatchets> for RegistryRatchetsSerde {
    fn from(value: RegistryRatchets) -> Self {
        let (sequence_present, sequence) = match value.sequence {
            RegistrySequence::Absent => (false, None),
            RegistrySequence::Present(sequence) => (true, Some(sequence)),
        };
        let (signature_present, certificate_identity, certificate_oidc_issuer) =
            value.signature.into_serialized();
        Self {
            sequence_present,
            sequence,
            valid_until_present: value.valid_until_present,
            yanked_versions: value.yanked_versions.into_serialized(),
            signature_present,
            certificate_identity,
            certificate_oidc_issuer,
            transparency_log_present: value.transparency_log_present,
        }
    }
}

impl<'de> Deserialize<'de> for RegistryRatchets {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        RegistryRatchetsSerde::deserialize(deserializer)?
            .try_into()
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RegistryLockSerde {
    resolved_hostname: String,
    sequence_present: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sequence: Option<u64>,
    sequence_anchor_established: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sequence_anchor: Option<u64>,
    valid_until_present: bool,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    yanked_versions: BTreeSet<String>,
    signature_present: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    certificate_identity: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    certificate_oidc_issuer: Option<String>,
    transparency_log_present: bool,
}

impl TryFrom<RegistryLockSerde> for RegistryLock {
    type Error = RegistryLockError;

    fn try_from(value: RegistryLockSerde) -> Result<Self, Self::Error> {
        let sequence = match (value.sequence_present, value.sequence) {
            (false, None) => RegistrySequence::Absent,
            (true, Some(sequence)) => RegistrySequence::Present(sequence),
            _ => return Err(RegistryLockError::InconsistentSequence),
        };
        let sequence_anchor = match (value.sequence_anchor_established, value.sequence_anchor) {
            (false, None) => RegistrySequenceAnchor::Unestablished,
            (true, Some(sequence)) => RegistrySequenceAnchor::Established(sequence),
            _ => return Err(RegistryLockError::InconsistentSequence),
        };
        match (&sequence, sequence_anchor) {
            (RegistrySequence::Absent, RegistrySequenceAnchor::Established(_)) => {
                return Err(RegistryLockError::InconsistentSequence);
            }
            (RegistrySequence::Present(observed), RegistrySequenceAnchor::Established(anchor))
                if anchor > *observed =>
            {
                return Err(RegistryLockError::InconsistentSequence);
            }
            (RegistrySequence::Absent | RegistrySequence::Present(_), _) => {}
        }
        let signature = RegistrySignatureProtection::from_serialized(
            value.signature_present,
            value.certificate_identity,
            value.certificate_oidc_issuer,
        )?;
        Ok(Self {
            resolved_hostname: value.resolved_hostname,
            sequence,
            sequence_anchor,
            valid_until_present: value.valid_until_present,
            yanked_versions: YankedRegistryVersions::from_serialized(value.yanked_versions),
            signature,
            transparency_log_present: value.transparency_log_present,
        })
    }
}

impl From<RegistryLock> for RegistryLockSerde {
    fn from(value: RegistryLock) -> Self {
        let (sequence_present, sequence) = match value.sequence {
            RegistrySequence::Absent => (false, None),
            RegistrySequence::Present(sequence) => (true, Some(sequence)),
        };
        let (sequence_anchor_established, sequence_anchor) = match value.sequence_anchor {
            RegistrySequenceAnchor::Unestablished => (false, None),
            RegistrySequenceAnchor::Established(sequence) => (true, Some(sequence)),
        };
        let (signature_present, certificate_identity, certificate_oidc_issuer) =
            value.signature.into_serialized();
        Self {
            resolved_hostname: value.resolved_hostname,
            sequence_present,
            sequence,
            sequence_anchor_established,
            sequence_anchor,
            valid_until_present: value.valid_until_present,
            yanked_versions: value.yanked_versions.into_serialized(),
            signature_present,
            certificate_identity,
            certificate_oidc_issuer,
            transparency_log_present: value.transparency_log_present,
        }
    }
}

#[derive(Debug)]
enum RegistryLockError {
    InconsistentSequence,
    InconsistentSignature,
}

impl fmt::Display for RegistryLockError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InconsistentSequence => write!(
                f,
                "registry lock is inconsistent: the sequence observation and established sequence anchor must be encoded together, and an anchor cannot exceed its observation"
            ),
            Self::InconsistentSignature => write!(
                f,
                "registry lock is inconsistent: certificate_identity and certificate_oidc_issuer must be present together and only when signature_present is true"
            ),
        }
    }
}

impl std::error::Error for RegistryLockError {}

impl<'de> Deserialize<'de> for RegistryLock {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        RegistryLockSerde::deserialize(deserializer)?
            .try_into()
            .map_err(serde::de::Error::custom)
    }
}

impl RegistrySignatureProtection {
    fn from_serialized(
        signature_required: bool,
        certificate_identity: Option<String>,
        certificate_oidc_issuer: Option<String>,
    ) -> Result<Self, RegistryLockError> {
        match (
            signature_required,
            certificate_identity,
            certificate_oidc_issuer,
        ) {
            (false, None, None) => Ok(Self::NotRequired),
            (true, None, None) => Ok(Self::RequiredUnpinned),
            (true, Some(certificate_identity), Some(certificate_oidc_issuer)) => {
                Ok(Self::RequiredPinned(IdentityPin {
                    certificate_identity,
                    certificate_oidc_issuer,
                }))
            }
            _ => Err(RegistryLockError::InconsistentSignature),
        }
    }

    fn into_serialized(self) -> (bool, Option<String>, Option<String>) {
        match self {
            Self::NotRequired => (false, None, None),
            Self::RequiredUnpinned => (true, None, None),
            Self::RequiredPinned(pin) => (
                true,
                Some(pin.certificate_identity),
                Some(pin.certificate_oidc_issuer),
            ),
        }
    }

    fn is_required(&self) -> bool {
        matches!(self, Self::RequiredUnpinned | Self::RequiredPinned(_))
    }

    fn identity_pin(&self) -> Option<&IdentityPin> {
        match self {
            Self::RequiredPinned(pin) => Some(pin),
            Self::NotRequired | Self::RequiredUnpinned => None,
        }
    }

    fn expected_identity(&self) -> Option<ExpectedIdentity> {
        match self {
            Self::RequiredPinned(pin) => Some(ExpectedIdentity::pinned(
                pin.certificate_identity.clone(),
                pin.certificate_oidc_issuer.clone(),
            )),
            Self::NotRequired | Self::RequiredUnpinned => None,
        }
    }
}

impl RegistrySequence {
    fn value(&self) -> Option<u64> {
        match self {
            Self::Absent => None,
            Self::Present(sequence) => Some(*sequence),
        }
    }
}

impl RegistrySequenceAnchor {
    fn value(self) -> Option<u64> {
        match self {
            Self::Unestablished => None,
            Self::Established(sequence) => Some(sequence),
        }
    }
}

impl RegistryLock {
    pub fn resolved_hostname(&self) -> &str {
        &self.resolved_hostname
    }

    pub fn yanked_versions(&self) -> &YankedRegistryVersions {
        &self.yanked_versions
    }
}

/// The full carina-providers.lock file.
///
/// `LockFile` deliberately implements neither `Serialize` nor `Deserialize`.
/// Serialization stays behind the private seam used by [`Self::save`], while
/// deserialization is routed through [`Self::load`], which checks the format
/// version before the current schema can consume or rewrite the file.
#[derive(Debug, Clone)]
pub struct LockFile {
    version: u32,
    registry_host: BTreeMap<String, RegistryHostLock>,
    pub provider: Vec<LockEntry<RegistryLock>>,
    unpinned_registry_ratchets: UnpinnedRegistryRatchets,
}

/// The crate-internal serialization view of [`LockFile`]. Field order and
/// omission rules are part of the lock-file byte format and must stay aligned
/// with the former direct `LockFile` serialization.
#[derive(Serialize)]
struct SerializableLockFile<'a> {
    version: &'a u32,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    registry_host: &'a BTreeMap<String, RegistryHostLock>,
    provider: &'a Vec<LockEntry<RegistryLock>>,
    #[serde(default, skip_serializing_if = "UnpinnedRegistryRatchets::is_empty")]
    unpinned_registry_ratchets: &'a UnpinnedRegistryRatchets,
}

mod registry_lock_with_host {
    use super::*;

    /// A provider registry record paired with the host pin it references. This
    /// is the only result of converting verified discovery into persisted state.
    pub(super) struct RegistryLockWithHost {
        host: (String, RegistryHostLock),
        registry: RegistryLock,
    }

    impl RegistryLock {
        pub(super) fn from_resolved_registry(
            registry: ResolvedRegistry,
            ratchets: RegistryRatchets,
            validated_sequence: ValidatedRegistrySequence,
        ) -> RegistryLockWithHost {
            let ResolvedRegistry {
                hostname,
                discovery_pin,
            } = registry;
            let RegistryRatchets {
                sequence,
                valid_until_present,
                yanked_versions,
                signature,
                transparency_log_present,
            } = ratchets;
            let sequence_anchor = validated_sequence.into_anchor();
            RegistryLockWithHost {
                host: (hostname.clone(), RegistryHostLock::pinned(discovery_pin)),
                registry: Self {
                    resolved_hostname: hostname,
                    sequence,
                    sequence_anchor,
                    valid_until_present,
                    yanked_versions,
                    signature,
                    transparency_log_present,
                },
            }
        }
    }

    impl LockFile {
        /// Insert a registry provider together with the host record it references.
        pub(super) fn upsert_registry_with_host(
            &mut self,
            entry: LockEntry<NoRegistryLock>,
            registry_with_host: RegistryLockWithHost,
        ) {
            let RegistryLockWithHost {
                host: (hostname, host),
                registry,
            } = registry_with_host;
            self.registry_host.insert(hostname, host);

            let mut entry = entry.into_stored();
            entry.registry = Some(registry);
            self.upsert_entry(entry);
        }
    }
}

/// A validated identity re-pin that has not mutated its lock file yet.
/// The exclusive borrow keeps the previewed state unchanged until this value
/// is either consumed by [`Self::commit`] or dropped.
pub struct PreparedRegistryIdentityRepin<'a> {
    lock_file: &'a mut LockFile,
    source_key: String,
    target: RegistryRecoveryTarget<RegistryIdentityRepinState>,
}

/// A validated registry re-bootstrap that has not mutated its lock file yet.
/// The exclusive borrow keeps the previewed state unchanged until this value
/// is either consumed by [`Self::commit`] or dropped.
pub struct PreparedRegistryRebootstrap<'a> {
    lock_file: &'a mut LockFile,
    source_key: String,
    target: RegistryRecoveryTarget<RegistryRebootstrapState>,
}

/// A validated host discovery re-pin that holds the one resolved host record
/// used by commit together with the concrete values it will discard.
pub struct PreparedRegistryDiscoveryRepin<'a> {
    host: &'a mut RegistryHostLock,
    discarded_pin: RegistryDiscoveryPin,
}

#[derive(Deserialize)]
struct LockFileVersion {
    version: Option<u32>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UncheckedLockFile {
    version: u32,
    #[serde(default)]
    registry_host: BTreeMap<String, RegistryHostLock>,
    #[serde(default)]
    provider: Vec<LockEntry<RegistryLock>>,
    #[serde(default)]
    unpinned_registry_ratchets: UnpinnedRegistryRatchets,
}

#[derive(Debug)]
pub enum LockFileError {
    Read {
        path: PathBuf,
        source: io::Error,
    },
    MissingVersion {
        path: PathBuf,
    },
    VersionTooNew {
        found: u32,
        supported: u32,
    },
    VersionTooOld {
        found: u32,
        supported: u32,
    },
    MissingRegistryHostRecord {
        path: PathBuf,
        provider: String,
        hostname: String,
    },
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },
    RegistryIdentityPinConflict {
        path: PathBuf,
        source: Box<RegistryIdentityPinConflict>,
    },
}

impl fmt::Display for LockFileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read { path, source } => {
                write!(f, "Failed to read {}: {source}", path.display())
            }
            Self::MissingVersion { path } => write!(
                f,
                "Lock file {} has no format version and predates lock format versioning. It cannot be distinguished from a lock whose protection fields were stripped by an older Carina binary. Delete it, then regenerate it with `carina init`.",
                path.display()
            ),
            Self::VersionTooNew { found, supported } => write!(
                f,
                "Lock file version {found} is newer than supported version {supported}. Please upgrade Carina."
            ),
            Self::VersionTooOld { found, supported } => write!(
                f,
                "Lock file version {found} uses an older format that this Carina release cannot read; version {supported} is required. Delete the lock file, then run `carina init` to regenerate it. This re-establishes the pins by verifying registry discovery and the providers afresh; signing identity pins therefore return to first contact."
            ),
            Self::MissingRegistryHostRecord {
                path,
                provider,
                hostname,
            } => write!(
                f,
                "Lock file {} records registry provider {provider:?} as resolved through host {hostname:?}, but that host record is missing. The host record must be restored before a normal `carina init` can re-resolve against that host and re-establish the discovery pin.",
                path.display()
            ),
            Self::Parse { path, source } => write!(
                f,
                "Failed to parse {}: {source}\nhint: delete {} and re-run `carina init`.",
                path.display(),
                path.display()
            ),
            Self::RegistryIdentityPinConflict { path, source } => write!(
                f,
                "Lock file {} contains {source}; delete it and re-run `carina init`.",
                path.display()
            ),
        }
    }
}

impl std::error::Error for LockFileError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Read { source, .. } => Some(source),
            Self::Parse { source, .. } => Some(source),
            Self::RegistryIdentityPinConflict { source, .. } => Some(source),
            Self::MissingVersion { .. }
            | Self::VersionTooNew { .. }
            | Self::VersionTooOld { .. }
            | Self::MissingRegistryHostRecord { .. } => None,
        }
    }
}

#[derive(Debug)]
pub enum LockConstraintError {
    LockFile(LockFileError),
    ConstraintMismatch {
        provider: String,
        locked_version: String,
        constraint: String,
    },
}

impl fmt::Display for LockConstraintError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LockFile(error) => error.fmt(f),
            Self::ConstraintMismatch {
                provider,
                locked_version,
                constraint,
            } => write!(
                f,
                "Provider '{provider}' locked at version {locked_version}, but constraint '{constraint}' requires a different version.\nRun `carina init --upgrade` to resolve."
            ),
        }
    }
}

impl std::error::Error for LockConstraintError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::LockFile(error) => Some(error),
            Self::ConstraintMismatch { .. } => None,
        }
    }
}

impl From<LockFileError> for LockConstraintError {
    fn from(error: LockFileError) -> Self {
        Self::LockFile(error)
    }
}

impl Default for LockFile {
    fn default() -> Self {
        Self {
            version: Self::CURRENT_VERSION,
            registry_host: BTreeMap::new(),
            provider: Vec::new(),
            unpinned_registry_ratchets: UnpinnedRegistryRatchets::default(),
        }
    }
}

impl<'a> PreparedRegistryIdentityRepin<'a> {
    /// Return the identity pin that committing this recovery will discard.
    pub fn identity_pin(&self) -> &IdentityPin {
        &self.target.state().pin
    }

    /// Consume this prepared recovery and clear its previewed identity pin.
    pub fn commit(self) -> Result<(), RegistryLockRecoveryError> {
        let Self {
            lock_file,
            source_key,
            target,
        } = self;
        lock_file.commit_prepared_registry_identity_repin(source_key, target)
    }
}

impl PreparedRegistryRebootstrap<'_> {
    /// Return the observation and anchor that committing this recovery will discard.
    pub fn freshness(&self) -> RegistryFreshness {
        RegistryFreshness {
            sequence: self.target.state().sequence.value(),
            sequence_anchor: self.target.sequence_anchor().value(),
        }
    }

    /// Consume this prepared recovery and clear its previewed freshness pair.
    pub fn commit(self) -> Result<(), RegistryLockRecoveryError> {
        let Self {
            lock_file,
            source_key,
            target,
        } = self;
        lock_file.commit_prepared_registry_rebootstrap(source_key, target)
    }
}

impl PreparedRegistryDiscoveryRepin<'_> {
    /// Return the consumed host discovery values that commit will discard.
    pub fn discovery_pin(&self) -> &RegistryDiscoveryPin {
        &self.discarded_pin
    }

    /// Clear the consumed values without touching provider-owned state or
    /// pinned discovery values this client does not consume.
    pub fn commit(self) {
        let retained = self.host.additional_discovery_values().clone();
        self.host.discovery = RegistryDiscoveryPinState::Unpinned(retained);
    }
}

fn resolve_parent(path: &Path) -> &Path {
    // Empty parents (bare relative filenames) and paths with no parent (such as
    // roots) both fall back to the current directory. For bare filenames, this
    // keeps the temporary file on the target's mount.
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
}

// Linux allows at most 40 symlink traversals during pathname resolution. Use
// the same bound so dangling-chain fallback cannot loop forever on platforms
// where canonicalize does not diagnose a cycle first.
const MAX_LOCK_FILE_SYMLINK_HOPS: usize = 40;

#[cfg(unix)]
fn filesystem_loop_error() -> io::Error {
    // ErrorKind::FilesystemLoop is not yet directly constructible on stable
    // Rust, but std maps the platform's ELOOP code to that kind.
    io::Error::from_raw_os_error(libc::ELOOP)
}

#[cfg(windows)]
fn filesystem_loop_error() -> io::Error {
    // WinError.h's ERROR_CANT_RESOLVE_FILENAME maps to FilesystemLoop in std.
    const ERROR_CANT_RESOLVE_FILENAME: i32 = 1921;
    io::Error::from_raw_os_error(ERROR_CANT_RESOLVE_FILENAME)
}

fn resolve_lock_file_save_path(path: &Path) -> io::Result<PathBuf> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(path.to_path_buf());
        }
        Err(error) => return Err(error),
    };
    if !metadata.file_type().is_symlink() {
        return Ok(path.to_path_buf());
    }

    match fs::canonicalize(path) {
        Ok(target) => Ok(target),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let mut link_path = path.to_path_buf();
            for _ in 0..MAX_LOCK_FILE_SYMLINK_HOPS {
                let target = fs::read_link(&link_path)?;
                let target = if target.is_absolute() {
                    target
                } else {
                    resolve_parent(&link_path).join(target)
                };

                match fs::symlink_metadata(&target) {
                    Ok(metadata) if metadata.file_type().is_symlink() => {
                        link_path = target;
                    }
                    Ok(_) => return Ok(target),
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {
                        return Ok(target);
                    }
                    Err(error) => return Err(error),
                }
            }
            Err(filesystem_loop_error())
        }
        Err(error) => Err(error),
    }
}

#[cfg(test)]
type LockFileRenameHook = Box<dyn FnOnce(&Path, &Path) -> io::Result<()>>;

#[cfg(test)]
std::thread_local! {
    static LOCK_FILE_RENAME_HOOK: std::cell::RefCell<Option<LockFileRenameHook>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn rename_lock_file_for_test(from: &Path, to: &Path) -> io::Result<()> {
    if let Some(rename) = LOCK_FILE_RENAME_HOOK.with(|hook| hook.borrow_mut().take()) {
        return rename(from, to);
    }
    fs::rename(from, to)
}

impl LockFile {
    /// v3: Registry discovery pins contain the resolved values the client
    /// consumes, keyed by discovery field. The open map preserves values an
    /// older client does not consume.
    ///
    /// v2 pinned both the resolved API base and a hash of the discovery
    /// document bytes. It is rejected rather than reinterpreted as the v3
    /// values-only schema.
    pub const CURRENT_VERSION: u32 = 3;

    fn registry_host_lock(&self, hostname: &str) -> Option<&RegistryHostLock> {
        self.registry_host.get(hostname)
    }

    fn sources_match(existing: &str, requested: &str) -> bool {
        existing == requested || canonical_lock_source(existing) == canonical_lock_source(requested)
    }

    /// Load `carina-providers.lock`.
    ///
    /// Returns `Ok(None)` when the file is absent (normal first-run case).
    /// Parse errors — including an entry that can't be discriminated into a
    /// [`LockEntryKind`] variant — surface as `Err` rather than being silently
    /// collapsed into a default-empty lock.
    pub fn load(path: &Path) -> Result<Option<Self>, LockFileError> {
        let content = match fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(source) => {
                return Err(LockFileError::Read {
                    path: path.to_path_buf(),
                    source,
                });
            }
        };
        Self::from_toml_str(&content, path).map(Some)
    }

    /// Validated parse seam shared by filesystem loading and unit tests. It is
    /// private so callers cannot deserialize a `LockFile` around the version
    /// gate.
    fn from_toml_str(content: &str, path: &Path) -> Result<Self, LockFileError> {
        let version: LockFileVersion =
            toml::from_str(content).map_err(|source| LockFileError::Parse {
                path: path.to_path_buf(),
                source,
            })?;
        let version = version
            .version
            .ok_or_else(|| LockFileError::MissingVersion {
                path: path.to_path_buf(),
            })?;
        if version > Self::CURRENT_VERSION {
            return Err(LockFileError::VersionTooNew {
                found: version,
                supported: Self::CURRENT_VERSION,
            });
        }
        if version < Self::CURRENT_VERSION {
            // Older formats either hash discovery document bytes or keep
            // per-provider discovery copies, so they cannot be reinterpreted
            // as the current host-owned resolved-values map.
            return Err(LockFileError::VersionTooOld {
                found: version,
                supported: Self::CURRENT_VERSION,
            });
        }
        let unchecked: UncheckedLockFile =
            toml::from_str(content).map_err(|source| LockFileError::Parse {
                path: path.to_path_buf(),
                source,
            })?;
        let unpinned_registry_ratchets = unchecked
            .unpinned_registry_ratchets
            .into_canonical()
            .map_err(|source| LockFileError::RegistryIdentityPinConflict {
                path: path.to_path_buf(),
                source: Box::new(source),
            })?;
        for entry in &unchecked.provider {
            let Some(registry) = &entry.registry else {
                continue;
            };
            if !unchecked
                .registry_host
                .contains_key(registry.resolved_hostname())
            {
                return Err(LockFileError::MissingRegistryHostRecord {
                    path: path.to_path_buf(),
                    provider: canonical_lock_source(&entry.source),
                    hostname: registry.resolved_hostname().to_owned(),
                });
            }
        }
        let mut lock = Self {
            version: Self::CURRENT_VERSION,
            registry_host: unchecked.registry_host,
            provider: unchecked.provider,
            unpinned_registry_ratchets,
        };
        lock.attach_unpinned_registry_ratchets().map_err(|source| {
            LockFileError::RegistryIdentityPinConflict {
                path: path.to_path_buf(),
                source: Box::new(source),
            }
        })?;
        debug_assert_eq!(unchecked.version, version);
        Ok(lock)
    }

    fn to_toml_string(&self) -> Result<String, toml::ser::Error> {
        toml::to_string_pretty(&SerializableLockFile {
            version: &self.version,
            registry_host: &self.registry_host,
            provider: &self.provider,
            unpinned_registry_ratchets: &self.unpinned_registry_ratchets,
        })
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        // rename replaces a symlink itself, unlike fs::write. Resolve the
        // target first so every durability and replacement operation occurs
        // in the target directory while the caller's symlink remains intact.
        // It requires delete/replace permission for the target name, so a
        // deny-delete ACL intentionally makes save fail: no atomic replacement
        // can leave that name untouched.
        let path = resolve_lock_file_save_path(path)?;
        let parent = resolve_parent(&path);
        let content = self
            .to_toml_string()
            .map_err(|e| io::Error::other(format!("Failed to serialize lock file: {e}")))?;

        #[cfg(unix)]
        let mut temp_file = {
            use std::os::unix::fs::PermissionsExt;

            let existing_permissions = match fs::metadata(&path) {
                Ok(metadata) => Some(metadata.permissions()),
                Err(error) if error.kind() == io::ErrorKind::NotFound => None,
                Err(error) => return Err(error),
            };

            // Builder::tempfile_in is the configurable counterpart of
            // NamedTempFile::new_in: the file remains in the target directory,
            // while 0o666 is filtered by umask at creation just like fs::write.
            let mut builder = tempfile::Builder::new();
            builder.permissions(fs::Permissions::from_mode(0o666));
            let temp_file = builder.tempfile_in(parent)?;

            // Creation applies umask, so restore an existing target's exact
            // mode afterwards. The still-open file remains writable even when
            // the preserved mode is read-only.
            // Only mode is preserved; ownership, ACLs, and extended attributes
            // from the existing target are not retained by temp-file replacement.
            if let Some(permissions) = existing_permissions {
                temp_file.as_file().set_permissions(permissions)?;
            }
            temp_file
        };

        #[cfg(not(unix))]
        let mut temp_file = tempfile::NamedTempFile::new_in(parent)?;

        if let Err(error) = io::Write::write_all(&mut temp_file, content.as_bytes()) {
            let _ = temp_file.close();
            return Err(error);
        }
        if let Err(error) = temp_file.as_file().sync_all() {
            let _ = temp_file.close();
            return Err(error);
        }

        // Disarm automatic cleanup before the external rename so dropping the
        // tempfile cannot unlink a pathname concurrently reused after rename.
        let (temp_handle, temp_path) = match temp_file.keep() {
            Ok(parts) => parts,
            Err(error) => {
                let source = error.error;
                let _ = error.file.close();
                return Err(source);
            }
        };
        drop(temp_handle);
        #[cfg(test)]
        let rename_result = rename_lock_file_for_test(&temp_path, &path);
        #[cfg(not(test))]
        let rename_result = fs::rename(&temp_path, &path);
        if let Err(error) = rename_result {
            let _ = fs::remove_file(temp_path);
            return Err(error);
        }

        if let Ok(parent_dir) = fs::File::open(parent) {
            let _ = parent_dir.sync_all();
        }

        Ok(())
    }

    /// Find an entry matching `(source, version)` for modes with a published
    /// version pin. Git revision and file entries never match.
    pub fn find(&self, source: &str, version: &str) -> Option<&LockEntry<RegistryLock>> {
        self.provider.iter().find(|e| {
            Self::sources_match(&e.source, source) && e.kind.resolved_version() == Some(version)
        })
    }

    pub fn find_by_source(&self, source: &str) -> Option<&LockEntry<RegistryLock>> {
        self.provider
            .iter()
            .find(|e| Self::sources_match(&e.source, source))
    }

    fn resolve_registry_recovery(
        &self,
        provider: &str,
    ) -> Result<ResolvedRegistryRecovery, RegistryLockRecoveryError> {
        let source = parse_provider_source(provider).map_err(|reason| {
            RegistryLockRecoveryError::InvalidProvider {
                provider: provider.to_string(),
                reason,
            }
        })?;
        let source_key = match source {
            ProviderSource::Registry(source) => source.source_key(),
            ProviderSource::GithubDirect { .. } => {
                return Err(RegistryLockRecoveryError::NotRegistryProvider {
                    provider: provider.to_string(),
                });
            }
        };
        if let Some((index, registry)) =
            self.provider.iter().enumerate().find_map(|(index, entry)| {
                (Self::sources_match(&entry.source, &source_key))
                    .then_some(entry.registry.as_ref())
                    .flatten()
                    .map(|registry| (index, registry))
            })
        {
            let recorded = RegistryRatchets::from(registry);
            let state = match self.unpinned_registry_ratchets.get(&source_key) {
                Some(unpinned) => recorded
                    .merge(unpinned)
                    .map_err(RegistryLockRecoveryError::IdentityPinConflict)?,
                None => recorded,
            };
            return Ok(ResolvedRegistryRecovery {
                source_key,
                target: RegistryRecoveryTarget::Pinned {
                    index,
                    state,
                    sequence_anchor: registry.sequence_anchor,
                },
            });
        }
        let state = self
            .unpinned_registry_ratchets
            .get(&source_key)
            .cloned()
            .ok_or_else(|| RegistryLockRecoveryError::ProviderStateNotFound {
                provider: source_key.clone(),
            })?;
        Ok(ResolvedRegistryRecovery {
            source_key,
            target: RegistryRecoveryTarget::Unpinned { state },
        })
    }

    fn commit_prepared_registry_identity_repin(
        &mut self,
        source_key: String,
        target: RegistryRecoveryTarget<RegistryIdentityRepinState>,
    ) -> Result<(), RegistryLockRecoveryError> {
        match target {
            RegistryRecoveryTarget::Pinned { index, state, .. } => {
                let registry = self
                    .provider
                    .get_mut(index)
                    .and_then(|entry| entry.registry.as_mut())
                    .ok_or_else(|| RegistryLockRecoveryError::ProviderStateNotFound {
                        provider: source_key.clone(),
                    })?;
                state.residual.apply_to_registry(registry);
                self.unpinned_registry_ratchets.remove(&source_key);
            }
            RegistryRecoveryTarget::Unpinned { state } => {
                if !self
                    .unpinned_registry_ratchets
                    .commit_identity_repin(&source_key, state.residual)
                {
                    return Err(RegistryLockRecoveryError::ProviderStateNotFound {
                        provider: source_key,
                    });
                }
            }
        }
        Ok(())
    }

    fn commit_prepared_registry_rebootstrap(
        &mut self,
        source_key: String,
        target: RegistryRecoveryTarget<RegistryRebootstrapState>,
    ) -> Result<(), RegistryLockRecoveryError> {
        match target {
            RegistryRecoveryTarget::Pinned { index, state, .. } => {
                let registry = self
                    .provider
                    .get_mut(index)
                    .and_then(|entry| entry.registry.as_mut())
                    .ok_or_else(|| RegistryLockRecoveryError::ProviderStateNotFound {
                        provider: source_key.clone(),
                    })?;
                state.residual.apply_to_registry(registry);
                registry.sequence_anchor = RegistrySequenceAnchor::Unestablished;
                self.unpinned_registry_ratchets.remove(&source_key);
            }
            RegistryRecoveryTarget::Unpinned { state } => {
                if !self
                    .unpinned_registry_ratchets
                    .commit_rebootstrap(&source_key, state.residual)
                {
                    return Err(RegistryLockRecoveryError::ProviderStateNotFound {
                        provider: source_key,
                    });
                }
            }
        }
        Ok(())
    }

    /// Prepare an explicit identity re-pin without mutating the lock file.
    pub fn prepare_registry_identity_repin(
        &mut self,
        provider: &str,
    ) -> Result<PreparedRegistryIdentityRepin<'_>, RegistryLockRecoveryError> {
        let ResolvedRegistryRecovery { source_key, target } =
            self.resolve_registry_recovery(provider)?;
        let error_provider = source_key.clone();
        let target =
            target.try_map_state(|ratchets| ratchets.into_identity_repin_state(error_provider))?;
        Ok(PreparedRegistryIdentityRepin {
            lock_file: self,
            source_key,
            target,
        })
    }

    /// Prepare an explicit registry re-bootstrap without mutating the lock file.
    pub fn prepare_registry_rebootstrap(
        &mut self,
        provider: &str,
    ) -> Result<PreparedRegistryRebootstrap<'_>, RegistryLockRecoveryError> {
        let ResolvedRegistryRecovery { source_key, target } =
            self.resolve_registry_recovery(provider)?;
        let target = target.map_state(RegistryRatchets::into_rebootstrap_state);
        Ok(PreparedRegistryRebootstrap {
            lock_file: self,
            source_key,
            target,
        })
    }

    /// Prepare an explicit host discovery re-pin without mutating the lock.
    /// The returned exclusive borrow is the single lookup used for preview and
    /// commit, so the displayed host state cannot diverge from the write.
    pub fn prepare_registry_discovery_repin(
        &mut self,
        host: &str,
    ) -> Result<PreparedRegistryDiscoveryRepin<'_>, RegistryLockRecoveryError> {
        let host = host.to_ascii_lowercase();
        let host_lock = self.registry_host.get_mut(&host).ok_or_else(|| {
            RegistryLockRecoveryError::RegistryHostStateNotFound { host: host.clone() }
        })?;
        let discarded_pin = match &host_lock.discovery {
            RegistryDiscoveryPinState::Pinned(pin) => pin.consumed_values(),
            RegistryDiscoveryPinState::Unpinned(_) => {
                return Err(RegistryLockRecoveryError::DiscoveryAlreadyUnpinned { host });
            }
        };
        Ok(PreparedRegistryDiscoveryRepin {
            host: host_lock,
            discarded_pin,
        })
    }

    /// Find a revision-mode entry whose `resolved_sha` matches. Version and
    /// file entries can't have a resolved SHA, so they never match.
    pub fn find_by_source_and_sha(
        &self,
        source: &str,
        sha: &str,
    ) -> Option<&LockEntry<RegistryLock>> {
        self.provider.iter().find(|e| {
            Self::sources_match(&e.source, source)
                && matches!(
                    &e.kind,
                    LockEntryKind::Revision { resolved_sha, .. } if resolved_sha == sha
                )
        })
    }

    fn known_registry_ratchets(
        &self,
        source: &str,
    ) -> Result<RegistryRatchets, RegistryIdentityPinConflict> {
        let recorded = self
            .find_by_source(source)
            .and_then(|entry| entry.registry.as_ref())
            .map(RegistryRatchets::from)
            .unwrap_or_default();
        let source_key = canonical_lock_source(source);
        match self.unpinned_registry_ratchets.get(&source_key) {
            Some(unpinned) => recorded.merge(unpinned),
            None => Ok(recorded),
        }
    }

    fn registry_sequence_anchor(&self, source: &str) -> RegistrySequenceAnchor {
        self.find_by_source(source)
            .and_then(|entry| entry.registry.as_ref())
            .map(|registry| registry.sequence_anchor)
            .unwrap_or(RegistrySequenceAnchor::Unestablished)
    }

    fn attach_unpinned_registry_ratchets(&mut self) -> Result<(), RegistryIdentityPinConflict> {
        for entry in &mut self.provider {
            let Some(registry) = entry.registry.as_mut() else {
                continue;
            };
            let source_key = canonical_lock_source(&entry.source);
            if let Some(unpinned) = self.unpinned_registry_ratchets.remove(&source_key) {
                unpinned.merge_into_registry(registry)?;
            }
        }
        Ok(())
    }

    fn store_registry_ratchets(
        &mut self,
        source: &str,
        ratchets: RegistryRatchets,
    ) -> Result<(), RegistryIdentityPinConflict> {
        let source_key = canonical_lock_source(source);
        if let Some(registry) = self
            .provider
            .iter_mut()
            .find(|entry| Self::sources_match(&entry.source, &source_key))
            .and_then(|entry| entry.registry.as_mut())
        {
            ratchets.merge_into_registry(registry)?;
            self.unpinned_registry_ratchets.remove(&source_key);
        } else {
            self.unpinned_registry_ratchets
                .merge(source_key, ratchets)?;
        }
        Ok(())
    }

    pub fn upsert(&mut self, entry: LockEntry<NoRegistryLock>) {
        self.upsert_entry(entry.into_stored());
    }

    #[cfg(test)]
    fn upsert_registry(
        &mut self,
        mut entry: LockEntry<RegistryLock>,
        host: RegistryHostLock,
    ) -> Result<(), RegistryIdentityPinConflict> {
        if let Some(registry) = entry.registry.as_mut() {
            let observed = self.known_registry_ratchets(&entry.source)?;
            observed.merge_into_registry(registry)?;
            self.registry_host
                .insert(registry.resolved_hostname().to_owned(), host);
        }
        self.upsert_entry(entry);
        Ok(())
    }

    fn upsert_entry(&mut self, entry: LockEntry<RegistryLock>) {
        if entry.registry.is_some() {
            self.unpinned_registry_ratchets
                .remove(&canonical_lock_source(&entry.source));
        }
        if let Some(existing) = self
            .provider
            .iter_mut()
            .find(|e| Self::sources_match(&e.source, &entry.source))
        {
            *existing = entry;
        } else {
            self.provider.push(entry);
        }
    }
}

/// The non-ratchet fields and validated sequence proof needed for a final
/// registry provider entry. Persisted ratchets remain absent: the upsert can
/// obtain them only from [`PersistentLockFile`].
struct RegistryProviderLockEntry {
    name: String,
    source: String,
    kind: LockEntryKind,
    sha256: String,
    registry: ResolvedRegistry,
    validated_sequence: ValidatedRegistrySequence,
}

/// A lock-file handle whose ratchet operations include the write that makes
/// accepted observations durable. The live [`LockFile`] is replaced only
/// after that write succeeds, so accepted registry security observations
/// cannot exist only in memory.
struct PersistentLockFile<'a> {
    lock_file: &'a mut LockFile,
    path: PathBuf,
}

impl<'a> PersistentLockFile<'a> {
    fn new(lock_file: &'a mut LockFile, path: PathBuf) -> Self {
        Self { lock_file, path }
    }

    fn lock_file(&self) -> &LockFile {
        self.lock_file
    }

    fn persist_registry_ratchets(
        &mut self,
        source: &str,
        observation: &str,
        update: impl FnOnce(&mut RegistryRatchets) -> Result<(), String>,
    ) -> Result<(), String> {
        let mut persisted = self.lock_file.clone();
        let source_key = canonical_lock_source(source);
        let mut ratchets = persisted
            .known_registry_ratchets(&source_key)
            .map_err(|error| error.to_string())?;
        update(&mut ratchets)?;
        persisted
            .store_registry_ratchets(&source_key, ratchets)
            .map_err(|error| error.to_string())?;
        persisted.save(&self.path).map_err(|error| {
            format!(
                "Failed to persist registry {observation} observation to {}: {error}",
                self.path.display()
            )
        })?;
        *self.lock_file = persisted;
        Ok(())
    }

    fn validate_and_record_registry_listing(
        &mut self,
        source: &RegistrySource,
        versions: &RegistryVersions,
    ) -> Result<ValidatedRegistrySequence, String> {
        let source_key = source.source_key();
        let locked = self
            .lock_file
            .known_registry_ratchets(&source_key)
            .map_err(|error| error.to_string())?;
        let sequence_anchor = self.lock_file.registry_sequence_anchor(&source_key);
        let validated =
            ValidatedRegistryListing::validate(source, versions, &locked, sequence_anchor)?;
        let (observations, validated_sequence) = validated.into_parts();

        if let Some(observations) = observations {
            self.persist_registry_ratchets(&source_key, "validated listing", |ratchets| {
                *ratchets = ratchets
                    .clone()
                    .merge(&observations)
                    .map_err(|error| error.to_string())?;
                Ok(())
            })?;
        }
        Ok(validated_sequence)
    }

    fn record_registry_transparency_log_presence(
        &mut self,
        source: &str,
        present: bool,
    ) -> Result<(), String> {
        if !present {
            return Ok(());
        }
        self.persist_registry_ratchets(source, "transparency_log presence", |ratchets| {
            ratchets.transparency_log_present = true;
            Ok(())
        })
    }

    fn record_verified_registry_signature(
        &mut self,
        source: &str,
        pin: IdentityPin,
    ) -> Result<(), String> {
        let verified = RegistryRatchets {
            signature: RegistrySignatureProtection::RequiredPinned(pin),
            ..RegistryRatchets::default()
        };
        self.persist_registry_ratchets(source, "verified signature", |ratchets| {
            *ratchets = ratchets
                .clone()
                .merge(&verified)
                .map_err(|error| error.to_string())?;
            Ok(())
        })
    }

    fn upsert_registry_provider(&mut self, entry: RegistryProviderLockEntry) -> Result<(), String> {
        let RegistryProviderLockEntry {
            name,
            source,
            kind,
            sha256,
            registry,
            validated_sequence,
        } = entry;
        let ratchets = self
            .lock_file
            .known_registry_ratchets(&source)
            .map_err(|error| error.to_string())?;
        let registry_with_host =
            RegistryLock::from_resolved_registry(registry, ratchets, validated_sequence);
        self.lock_file.upsert_registry_with_host(
            LockEntry {
                name,
                source,
                kind,
                sha256,
                registry: None,
            },
            registry_with_host,
        );
        Ok(())
    }
}

/// An installed provider artifact together with the resolution provenance that
/// selected it. The path is private so load callers must retain this wrapper
/// and therefore cannot accidentally separate an artifact from its lock mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledProvider {
    path: PathBuf,
    provider_name: String,
    provenance: ProviderArtifactProvenance,
}

impl InstalledProvider {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn provenance(&self) -> &ProviderArtifactProvenance {
        &self.provenance
    }

    pub fn provider_name(&self) -> &str {
        &self.provider_name
    }

    /// Attach this artifact's provenance to a load failure without flattening
    /// the underlying error. Display always renders the load error first.
    pub fn with_load_error<E>(&self, error: E) -> ProviderArtifactLoadError<E> {
        ProviderArtifactLoadError {
            error,
            provider_name: self.provider_name.clone(),
            provenance: Box::new(self.provenance.clone()),
        }
    }

    /// Load through a caller-supplied async function while making provenance
    /// attachment mandatory on the error path.
    pub async fn load_with<T, E, F, Fut>(
        &self,
        loader: F,
    ) -> Result<T, ProviderArtifactLoadError<E>>
    where
        F: FnOnce(PathBuf) -> Fut,
        Fut: std::future::Future<Output = Result<T, E>>,
    {
        loader(self.path.clone())
            .await
            .map_err(|error| self.with_load_error(error))
    }
}

/// Where an installed provider artifact came from. Locked variants cannot
/// represent `file://`, so only lock-controlled artifacts can produce a stale
/// lock hint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderArtifactProvenance {
    LockFile {
        lock_path: PathBuf,
        pin: LockedProviderPin,
    },
    File {
        source: String,
    },
}

/// The lock-entry modes that can select a cached provider artifact.
/// [`LockEntryKind::File`] is deliberately absent: file artifacts have their
/// own provenance variant and are never candidates for a stale-lock hint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LockedProviderPin {
    Version {
        version: String,
        constraint: Option<String>,
    },
    Revision {
        revision: String,
        resolved_sha: String,
    },
    RegistryRevision {
        revision: String,
        version: String,
    },
}

impl fmt::Display for LockedProviderPin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LockedProviderPin::Version {
                version,
                constraint,
            } => {
                write!(f, "version {version}")?;
                if let Some(constraint) = constraint {
                    write!(f, ", constraint {constraint}")?;
                }
                Ok(())
            }
            LockedProviderPin::Revision {
                revision,
                resolved_sha,
            } => write!(f, "revision {revision}, resolved_sha {resolved_sha}"),
            LockedProviderPin::RegistryRevision { revision, version } => {
                write!(f, "registry revision {revision}, version {version}")
            }
        }
    }
}

impl fmt::Display for ProviderArtifactProvenance {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProviderArtifactProvenance::LockFile { lock_path, pin } => {
                write!(
                    f,
                    "provider resolved from {} ({pin}); if this lock is stale, run `carina init --upgrade`",
                    lock_path.display()
                )
            }
            ProviderArtifactProvenance::File { source } => write!(
                f,
                "provider resolved from {source}; file providers are not controlled by carina-providers.lock"
            ),
        }
    }
}

/// A provider load failure that retains both the typed underlying error and
/// the artifact provenance selected by the resolver.
#[derive(Debug)]
pub struct ProviderArtifactLoadError<E> {
    error: E,
    provider_name: String,
    provenance: Box<ProviderArtifactProvenance>,
}

impl<E: fmt::Display> fmt::Display for ProviderArtifactLoadError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}\nProvider: {}\n{}",
            self.error, self.provider_name, self.provenance
        )
    }
}

// `Display` is the sole rendering surface for this composite diagnostic: CLI
// and LSP callers require one self-contained message containing both the child
// error and provenance. Exposing the same child through `source()` would make
// standard chain walkers print it again.
impl<E> std::error::Error for ProviderArtifactLoadError<E> where E: std::error::Error + 'static {}

fn missing_locked_artifact_error(
    base_dir: &Path,
    lock_path: &Path,
    pin: &LockedProviderPin,
    requested_revision: Option<&str>,
) -> String {
    let action = match requested_revision {
        Some(revision) => format!(
            "not installed. Run `carina init` in {} to install (revision: {revision})",
            base_dir.display()
        ),
        None => format!("not installed. Run `carina init` in {}", base_dir.display()),
    };
    format!(
        "{action}\nConsulted {} ({pin}), but its artifact is missing.",
        lock_path.display()
    )
}

/// Detect the current platform's target triple.
pub fn detect_target() -> Result<String, String> {
    let arch = std::env::consts::ARCH;
    let os = std::env::consts::OS;

    let target = match (arch, os) {
        ("aarch64", "macos") => "aarch64-apple-darwin",
        ("x86_64", "macos") => "x86_64-apple-darwin",
        ("x86_64", "linux") => "x86_64-unknown-linux-gnu",
        ("aarch64", "linux") => "aarch64-unknown-linux-gnu",
        _ => return Err(format!("Unsupported platform: {arch}-{os}")),
    };

    Ok(target.to_string())
}

/// Construct the download URL for a provider binary.
pub fn download_url(source: &str, version: &str, target: &str) -> Result<String, String> {
    let parts: Vec<&str> = source.split('/').collect();
    if parts.len() != 3 || parts[0] != "github.com" {
        return Err(format!(
            "Invalid source format: {source}. Expected: github.com/{{owner}}/{{repo}}"
        ));
    }
    let owner = parts[1];
    let repo = parts[2];

    Ok(format!(
        "https://github.com/{owner}/{repo}/releases/download/v{version}/{repo}-v{version}-{target}.tar.gz"
    ))
}

/// Construct the download URL for a WASM provider binary.
pub fn download_url_wasm(source: &str, version: &str) -> Result<String, String> {
    let parts: Vec<&str> = source.split('/').collect();
    if parts.len() != 3 || parts[0] != "github.com" {
        return Err(format!(
            "Invalid source format: {source}. Expected: github.com/{{owner}}/{{repo}}"
        ));
    }
    let owner = parts[1];
    let repo = parts[2];

    Ok(format!(
        "https://github.com/{owner}/{repo}/releases/download/v{version}/{repo}-v{version}.wasm"
    ))
}

const DEFAULT_REGISTRY_HOST: &str = "registry.carina-rs.dev";
// Registry sequence advances only when the canonical version-set digest changes;
// timestamp-only re-issues reuse it. Once a successful resolve establishes an
// anchor, later successful pins can advance it by at most this amount. A failed
// observation cannot move that anchor, and true first contact has no numeric base.
const MAX_SEQUENCE_FAST_FORWARD: u64 = 1_000_000;
const MAX_SIGNATURE_BUNDLE_BYTES: usize = 1024 * 1024;
const IDENTITY_REPIN_REMEDIATION: &str = "After verifying out-of-band that the signing-identity change is intended, run `carina providers repin-identity <provider>` to clear only the identity pin, then re-run `carina init` to acquire and verify a new pin.";
const SEQUENCE_REBOOTSTRAP_REMEDIATION: &str = "After verifying out-of-band that resetting registry freshness is intended, run `carina providers re-bootstrap <provider>` to clear only the persisted sequence observation and anchor, then re-run `carina init`.";
const DISCOVERY_REPIN_REMEDIATION: &str = "After verifying out-of-band that the change to the pinned discovery values (today, the resolved API base) is intended, run `carina providers repin-discovery <host>` to clear only those host discovery values, then re-run `carina init` to acquire and verify new values.";

fn recovery_remediation(template: &str, target: &str) -> String {
    template
        .replace("<provider>", target)
        .replace("<host>", target)
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ProviderSource {
    GithubDirect { source: String },
    Registry(RegistrySource),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RegistrySource {
    source: String,
    hostname: String,
    namespace: String,
    name: String,
}

/// A registry whose hostname and API base were resolved through §1 discovery.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ResolvedRegistry {
    hostname: String,
    discovery_pin: RegistryDiscoveryPin,
}

impl ResolvedRegistry {
    fn api_base_url(&self) -> &str {
        self.discovery_pin.api_base_url()
    }
}

#[derive(Debug, Clone, Copy)]
enum RegistryHttpRequest<'a> {
    Discovery(&'a str),
    Resource(&'a str),
}

impl<'a> RegistryHttpRequest<'a> {
    fn url(self) -> &'a str {
        match self {
            Self::Discovery(url) | Self::Resource(url) => url,
        }
    }
}

#[derive(Debug, Clone)]
enum HttpResponse {
    Success { body: Vec<u8> },
    Failure { status: u16 },
}

trait RegistryHttp {
    fn get(&self, request: RegistryHttpRequest<'_>) -> Result<HttpResponse, String>;

    fn download_to_file(&self, url: &str, dest: &Path) -> Result<(), String> {
        let body = match self.get(RegistryHttpRequest::Resource(url))? {
            HttpResponse::Success { body } => body,
            HttpResponse::Failure { status } => {
                return Err(format!("Download failed with status {status}: {url}"));
            }
        };
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("Failed to create directory {}: {e}", parent.display()))?;
        }
        fs::write(dest, body)
            .map_err(|e| format!("Failed to write file {}: {e}", dest.display()))?;
        Ok(())
    }
}

struct UreqRegistryHttp;

impl UreqRegistryHttp {
    fn agent(request: RegistryHttpRequest<'_>) -> ureq::Agent {
        match request {
            RegistryHttpRequest::Discovery(_) => ureq::Agent::config_builder()
                .max_redirects(0)
                .build()
                .into(),
            RegistryHttpRequest::Resource(_) => ureq::Agent::new_with_defaults(),
        }
    }
}

impl RegistryHttp for UreqRegistryHttp {
    fn get(&self, request: RegistryHttpRequest<'_>) -> Result<HttpResponse, String> {
        let url = request.url();
        let response = Self::agent(request).get(url).call();
        let response = match response {
            Ok(response) => response,
            Err(ureq::Error::StatusCode(status)) => {
                return Ok(HttpResponse::Failure { status });
            }
            Err(e) => return Err(format!("Failed to fetch {url}: {e}")),
        };
        let status = response.status().into();
        if !(200..300).contains(&status) {
            return Ok(HttpResponse::Failure { status });
        }
        let body = response
            .into_body()
            .read_to_vec()
            .map_err(|e| format!("Failed to read response body from {url}: {e}"))?;
        Ok(HttpResponse::Success { body })
    }

    fn download_to_file(&self, url: &str, dest: &Path) -> Result<(), String> {
        let response = ureq::get(url)
            .call()
            .map_err(|e| format!("Failed to download {url}: {e}"))?;

        if response.status() != 200 {
            return Err(format!(
                "Download failed with status {}: {url}",
                response.status()
            ));
        }

        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("Failed to create directory {}: {e}", parent.display()))?;
        }

        let mut reader = response.into_body().into_reader();
        let mut file = fs::File::create(dest)
            .map_err(|e| format!("Failed to create file {}: {e}", dest.display()))?;
        io::copy(&mut reader, &mut file)
            .map_err(|e| format!("Failed to write file {}: {e}", dest.display()))?;

        Ok(())
    }
}

#[derive(Debug, Deserialize)]
struct DiscoveryDocument {
    #[serde(rename = "providers.v1")]
    providers_v1: String,
}

#[derive(Debug, Deserialize)]
struct RegistryVersions {
    sequence: Option<u64>,
    valid_until: Option<String>,
    versions: Vec<RegistryVersion>,
}

#[derive(Debug, Deserialize)]
struct RegistryVersion {
    version: String,
    #[serde(default)]
    yanked: bool,
}

mod registry_listing_validation {
    use super::*;

    /// The sequence from a listing that passed every listing-level check.
    /// Only this proof can establish the anchor written by a successful pin.
    pub(super) struct ValidatedRegistrySequence(RegistrySequence);

    impl ValidatedRegistrySequence {
        pub(super) fn into_anchor(self) -> RegistrySequenceAnchor {
            match self.0 {
                RegistrySequence::Absent => RegistrySequenceAnchor::Unestablished,
                RegistrySequence::Present(sequence) => {
                    RegistrySequenceAnchor::Established(sequence)
                }
            }
        }
    }

    /// Listing observations that can only be constructed after every listing
    /// check has accepted the response.
    pub(super) struct ValidatedRegistryListing {
        observations: Option<RegistryRatchets>,
        sequence: ValidatedRegistrySequence,
    }

    impl ValidatedRegistryListing {
        pub(super) fn validate(
            source: &RegistrySource,
            versions: &RegistryVersions,
            locked: &RegistryRatchets,
            sequence_anchor: RegistrySequenceAnchor,
        ) -> Result<Self, String> {
            let valid_until = versions
                .valid_until
                .as_deref()
                .map(|raw_valid_until| {
                    OffsetDateTime::parse(raw_valid_until, &Rfc3339)
                        .map_err(|error| format!("Invalid registry valid_until timestamp: {error}"))
                })
                .transpose()?;
            let valid_until_present = valid_until.is_some();

            match sequence_anchor {
                RegistrySequenceAnchor::Unestablished => {}
                RegistrySequenceAnchor::Established(previous) => {
                    let Some(sequence) = versions.sequence else {
                        return Err(format!(
                            "registry sequence field disappeared for {}/{}",
                            source.namespace, source.name
                        ));
                    };
                    if sequence < previous {
                        let remediation = recovery_remediation(
                            SEQUENCE_REBOOTSTRAP_REMEDIATION,
                            &source.source_key(),
                        );
                        return Err(format!(
                            "registry sequence rollback for {}/{}: previous {}, got {}. {}",
                            source.namespace, source.name, previous, sequence, remediation
                        ));
                    }
                    if sequence.saturating_sub(previous) > MAX_SEQUENCE_FAST_FORWARD {
                        let remediation = recovery_remediation(
                            SEQUENCE_REBOOTSTRAP_REMEDIATION,
                            &source.source_key(),
                        );
                        return Err(format!(
                            "registry sequence fast-forward for {}/{} is too large: established anchor {}, got {}. {}",
                            source.namespace, source.name, previous, sequence, remediation
                        ));
                    }
                }
            }

            let stripped_versions = locked.yanked_versions.stripped_from(&versions.versions);
            if !stripped_versions.is_empty() {
                return Err(format!(
                    "registry yanked flag disappeared for {}/{} version(s): {}",
                    source.namespace,
                    source.name,
                    stripped_versions.join(", ")
                ));
            }
            if locked.valid_until_present && !valid_until_present {
                return Err(format!(
                    "registry valid_until field disappeared for {}/{}",
                    source.namespace, source.name
                ));
            }
            if valid_until.is_some_and(|valid_until| valid_until < OffsetDateTime::now_utc()) {
                return Err(format!(
                    "registry versions listing valid_until is expired for {}/{}",
                    source.namespace, source.name
                ));
            }

            let sequence = versions
                .sequence
                .map(RegistrySequence::Present)
                .unwrap_or(RegistrySequence::Absent);
            let yanked_versions =
                YankedRegistryVersions::default().with_observed(&versions.versions);
            let has_observations =
                versions.sequence.is_some() || valid_until_present || !yanked_versions.is_empty();
            let observations = has_observations.then_some(RegistryRatchets {
                sequence: sequence.clone(),
                valid_until_present,
                yanked_versions,
                signature: RegistrySignatureProtection::NotRequired,
                transparency_log_present: false,
            });
            Ok(Self {
                observations,
                sequence: ValidatedRegistrySequence(sequence),
            })
        }

        pub(super) fn into_parts(self) -> (Option<RegistryRatchets>, ValidatedRegistrySequence) {
            (self.observations, self.sequence)
        }
    }
}

use registry_listing_validation::{ValidatedRegistryListing, ValidatedRegistrySequence};

mod registry_version_candidates {
    use std::collections::HashSet;

    use super::{RegistryVersion, Version};

    /// Parsed registry versions that are structurally safe to select.
    pub(super) struct SelectableRegistryVersions {
        versions: Vec<SelectableRegistryVersion>,
        yanked_versions: Vec<Version>,
    }

    struct SelectableRegistryVersion(Version);

    impl SelectableRegistryVersion {
        fn from_listing(
            entry: &RegistryVersion,
            yanked_version_names: &HashSet<&str>,
        ) -> Option<Self> {
            if entry.yanked || yanked_version_names.contains(entry.version.as_str()) {
                return None;
            }
            Version::parse(&entry.version).ok().map(Self)
        }
    }

    impl SelectableRegistryVersions {
        pub(super) fn from_listing(entries: &[RegistryVersion]) -> Self {
            let yanked_version_names: HashSet<&str> = entries
                .iter()
                .filter(|entry| entry.yanked)
                .map(|entry| entry.version.as_str())
                .collect();
            Self {
                versions: entries
                    .iter()
                    .filter_map(|entry| {
                        SelectableRegistryVersion::from_listing(entry, &yanked_version_names)
                    })
                    .collect(),
                yanked_versions: yanked_version_names
                    .into_iter()
                    .filter_map(|version| Version::parse(version).ok())
                    .collect(),
            }
        }

        pub(super) fn iter(&self) -> impl Iterator<Item = &Version> {
            self.versions.iter().map(|version| &version.0)
        }

        pub(super) fn yanked_matching(
            &self,
            mut predicate: impl FnMut(&Version) -> bool,
        ) -> Vec<String> {
            let mut versions: Vec<Version> = self
                .yanked_versions
                .iter()
                .filter(|version| predicate(version))
                .cloned()
                .collect();
            versions.sort_by(|left, right| right.cmp(left));
            versions.dedup();
            versions
                .into_iter()
                .map(|version| version.to_string())
                .collect()
        }
    }
}

use registry_version_candidates::SelectableRegistryVersions;

#[derive(Debug, Deserialize)]
struct RegistryDownload {
    download_url: String,
    shasum: String,
    #[serde(default, deserialize_with = "deserialize_registry_signature")]
    signature: Option<RegistrySignature>,
    #[serde(default)]
    transparency_log: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct RegistrySignature {
    r#type: String,
    certificate_identity: String,
    certificate_oidc_issuer: String,
    bundle_url: String,
}

fn deserialize_registry_signature<'de, D>(
    deserializer: D,
) -> Result<Option<RegistrySignature>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    RegistrySignature::deserialize(deserializer)
        .map(Some)
        .map_err(|error| {
            serde::de::Error::custom(signing::verification_failure(format!(
                "malformed registry signature block: {error}"
            )))
        })
}

fn parse_provider_source(source: &str) -> Result<ProviderSource, String> {
    let parts: Vec<&str> = source.split('/').collect();
    match parts.as_slice() {
        ["github.com", _owner, _repo] => Ok(ProviderSource::GithubDirect {
            source: source.to_string(),
        }),
        [namespace, name] if !namespace.is_empty() && !name.is_empty() => {
            Ok(ProviderSource::Registry(RegistrySource {
                source: format!("{namespace}/{name}"),
                hostname: DEFAULT_REGISTRY_HOST.to_string(),
                namespace: (*namespace).to_string(),
                name: (*name).to_string(),
            }))
        }
        [hostname, namespace, name]
            if !hostname.is_empty() && !namespace.is_empty() && !name.is_empty() =>
        {
            let hostname = hostname.to_ascii_lowercase();
            let source = canonical_registry_source(&hostname, namespace, name);
            Ok(ProviderSource::Registry(RegistrySource {
                source,
                hostname,
                namespace: (*namespace).to_string(),
                name: (*name).to_string(),
            }))
        }
        _ => Err(format!(
            "Invalid source format: {source}. Expected: github.com/{{owner}}/{{repo}} or [hostname/]namespace/name"
        )),
    }
}

fn canonical_provider_source(source: &str) -> Result<String, String> {
    Ok(parse_provider_source(source)?.source_key())
}

fn canonical_registry_source(hostname: &str, namespace: &str, name: &str) -> String {
    if hostname == DEFAULT_REGISTRY_HOST {
        format!("{namespace}/{name}")
    } else {
        format!("{hostname}/{namespace}/{name}")
    }
}

fn canonical_lock_source(source: &str) -> String {
    let parts: Vec<&str> = source.split('/').collect();
    match parts.as_slice() {
        ["github.com", _owner, _repo] => source.to_string(),
        [namespace, name] if !namespace.is_empty() && !name.is_empty() => {
            format!("{namespace}/{name}")
        }
        [hostname, namespace, name]
            if !hostname.is_empty() && !namespace.is_empty() && !name.is_empty() =>
        {
            canonical_registry_source(&hostname.to_ascii_lowercase(), namespace, name)
        }
        _ => source.to_string(),
    }
}

fn registry_revision<'a>(
    source: &str,
    config: &'a ProviderConfig,
) -> Result<Option<&'a str>, String> {
    let Some(revision) = config.revision.as_deref() else {
        return Ok(None);
    };
    match parse_provider_source(source)? {
        ProviderSource::Registry(_) => Ok(Some(revision)),
        ProviderSource::GithubDirect { .. } => Ok(None),
    }
}

fn fetch_json<T: for<'de> Deserialize<'de>, H: RegistryHttp>(
    http: &H,
    url: &str,
) -> Result<T, String> {
    let body = match http.get(RegistryHttpRequest::Resource(url))? {
        HttpResponse::Success { body } => body,
        HttpResponse::Failure { status } => {
            return Err(format!(
                "Registry request failed with status {status}: {url}"
            ));
        }
    };
    let parsed = serde_json::from_slice(&body)
        .map_err(|e| format!("Failed to parse registry JSON from {url}: {e}"))?;
    Ok(parsed)
}

fn fetch_discovery_json<T: for<'de> Deserialize<'de>, H: RegistryHttp>(
    http: &H,
    url: &str,
) -> Result<T, String> {
    let body = match http.get(RegistryHttpRequest::Discovery(url))? {
        HttpResponse::Success { body } => body,
        HttpResponse::Failure { status } if (300..400).contains(&status) => {
            return Err(format!(
                "Registry discovery fetch failed: redirect status {status} from {url}"
            ));
        }
        HttpResponse::Failure { status } => {
            return Err(format!(
                "Registry discovery fetch failed with status {status}: {url}"
            ));
        }
    };
    serde_json::from_slice(&body)
        .map_err(|e| format!("Failed to parse registry JSON from {url}: {e}"))
}

fn join_registry_url(base: &str, path: &str) -> String {
    format!("{}{}", base.trim_end_matches('/'), path)
}

fn same_consumed_discovery_values(
    left: &RegistryDiscoveryPin,
    right: &RegistryDiscoveryPin,
) -> bool {
    left.consumed_values() == right.consumed_values()
}

fn resolve_registry<H: RegistryHttp>(
    source: &RegistrySource,
    existing_host: Option<&RegistryHostLock>,
    http: &H,
) -> Result<ResolvedRegistry, String> {
    let discovery_url = registry_discovery_url(&source.hostname)?;
    let discovery: DiscoveryDocument = fetch_discovery_json(http, discovery_url.as_str())?;
    let api_base_url = resolve_api_base_url(&discovery_url, &discovery.providers_v1)?;
    let additional = existing_host
        .map(RegistryHostLock::additional_discovery_values)
        .cloned()
        .unwrap_or_default();
    let discovery_pin = RegistryDiscoveryPin {
        api_base_url,
        additional,
    };
    if let Some(existing_pin) = existing_host.and_then(RegistryHostLock::pin)
        && !same_consumed_discovery_values(existing_pin, &discovery_pin)
    {
        let host = source.hostname.as_str();
        let remediation = recovery_remediation(DISCOVERY_REPIN_REMEDIATION, host);
        return Err(format!(
            "registry pinned discovery values mismatch for host {host}: pinned providers.v1 was {}; resolved providers.v1 is {}. {remediation}",
            existing_pin.api_base_url(),
            discovery_pin.api_base_url()
        ));
    }
    Ok(ResolvedRegistry {
        hostname: source.hostname.clone(),
        discovery_pin,
    })
}

fn registry_discovery_url(hostname: &str) -> Result<url::Url, String> {
    let mut discovery_url = url::Url::parse(&format!("https://{hostname}/")).map_err(|error| {
        format!("invalid registry hostname: expected a host with optional port: {error}")
    })?;
    if !discovery_url.username().is_empty()
        || discovery_url.password().is_some()
        || discovery_url.path() != "/"
        || discovery_url.query().is_some()
        || discovery_url.fragment().is_some()
    {
        return Err(
            "invalid registry hostname: expected a host with optional port and no URL components"
                .into(),
        );
    }
    discovery_url.set_path(REGISTRY_DISCOVERY_PATH);
    Ok(discovery_url)
}

fn resolve_api_base_url(discovery_url: &url::Url, providers_v1: &str) -> Result<String, String> {
    let api_base_url = discovery_url.join(providers_v1).map_err(|error| {
        format!("invalid registry discovery providers.v1 reference {providers_v1:?}: {error}")
    })?;
    if !is_absolute_https_api_base_url(&api_base_url) {
        return Err(format!(
            "registry discovery providers.v1 must use HTTPS: {providers_v1}"
        ));
    }
    if !api_base_url.username().is_empty() || api_base_url.password().is_some() {
        return Err("registry discovery providers.v1 API base must not contain userinfo".into());
    }
    if api_base_url.origin() != discovery_url.origin() {
        return Err(format!(
            "registry discovery returned cross-origin providers.v1: {providers_v1}"
        ));
    }
    if api_base_url.path() == discovery_url.path() {
        return Err(format!(
            "registry discovery providers.v1 must not resolve to the discovery document: {providers_v1:?}"
        ));
    }
    if api_base_url.query().is_some() || api_base_url.fragment().is_some() {
        return Err(
            "registry discovery providers.v1 API base must not contain a query or fragment".into(),
        );
    }
    Ok(ensure_trailing_slash(api_base_url).into())
}

fn validate_persisted_api_base_url(value: &str) -> Result<(), RegistryDiscoveryPinError> {
    let api_base_url =
        url::Url::parse(value).map_err(|source| RegistryDiscoveryPinError::InvalidApiBaseUrl {
            value: value.into(),
            source: Some(source),
        })?;
    if !is_absolute_https_api_base_url(&api_base_url) {
        return Err(RegistryDiscoveryPinError::InvalidApiBaseUrl {
            value: value.into(),
            source: None,
        });
    }
    Ok(())
}

fn is_absolute_https_api_base_url(url: &url::Url) -> bool {
    url.scheme() == "https" && url.has_host() && !url.cannot_be_a_base()
}

fn ensure_trailing_slash(mut url: url::Url) -> url::Url {
    if !url.path().ends_with('/') {
        let mut path = url.path().to_owned();
        path.push('/');
        url.set_path(&path);
    }
    url
}

struct RecordedRegistryListing {
    versions: RegistryVersions,
    validated_sequence: ValidatedRegistrySequence,
}

fn fetch_registry_versions<H: RegistryHttp>(
    registry: &ResolvedRegistry,
    source: &RegistrySource,
    lock_file: &mut PersistentLockFile<'_>,
    http: &H,
) -> Result<RecordedRegistryListing, String> {
    let url = join_registry_url(
        registry.api_base_url(),
        &format!("/{}/{}/versions", source.namespace, source.name),
    );
    let versions: RegistryVersions = fetch_json(http, &url)?;
    let validated_sequence = lock_file.validate_and_record_registry_listing(source, &versions)?;
    Ok(RecordedRegistryListing {
        versions,
        validated_sequence,
    })
}

impl RegistrySource {
    fn source_key(&self) -> String {
        self.source.clone()
    }
}

impl ProviderSource {
    fn source_key(&self) -> String {
        match self {
            ProviderSource::GithubDirect { source } => source.clone(),
            ProviderSource::Registry(source) => source.source_key(),
        }
    }
}

fn select_registry_version(
    versions: &SelectableRegistryVersions,
    config: &ProviderConfig,
) -> Result<String, String> {
    if let Some(revision) = &config.revision {
        return versions
            .iter()
            .filter(|version| registry_revision_matches(version, revision))
            .max()
            .map(|version| version.to_string())
            .ok_or_else(|| {
                registry_selection_error(
                    format!(
                        "No registry version of '{}' matches revision '{}'",
                        config.name, revision
                    ),
                    versions
                        .yanked_matching(|version| registry_revision_matches(version, revision)),
                )
            });
    }

    match &config.version {
        Some(constraint) => {
            let mut candidates: Vec<&Version> = versions
                .iter()
                .filter(|version| constraint.req.matches(version))
                .collect();
            candidates.sort_by(|a, b| b.cmp(a));
            candidates
                .into_iter()
                .next()
                .map(|version| version.to_string())
                .ok_or_else(|| {
                    registry_selection_error(
                        format!(
                            "No release of '{}' matches constraint '{}'",
                            config.name, constraint.raw
                        ),
                        versions.yanked_matching(|version| constraint.req.matches(version)),
                    )
                })
        }
        None => versions
            .iter()
            .max()
            .map(|version| version.to_string())
            .ok_or_else(|| {
                registry_selection_error(
                    format!("No versions found for provider '{}'", config.name),
                    versions.yanked_matching(|_| true),
                )
            }),
    }
}

fn registry_selection_error(message: String, yanked_versions: Vec<String>) -> String {
    if yanked_versions.is_empty() {
        message
    } else {
        format!(
            "{message}; yanked versions matching this request were skipped: {}",
            yanked_versions.join(", ")
        )
    }
}

fn registry_revision_matches(version: &Version, revision: &str) -> bool {
    if version.major != 0 || version.minor != 0 || version.patch != 0 {
        return false;
    }

    let mut identifiers = version.pre.as_str().split('.');
    matches!(identifiers.next(), Some(first) if first == revision)
        && matches!(identifiers.next(), Some(run) if !run.is_empty() && run.bytes().all(|b| b.is_ascii_digit()))
}

fn fetch_registry_download<H: RegistryHttp>(
    registry: &ResolvedRegistry,
    source: &RegistrySource,
    version: &str,
    http: &H,
) -> Result<RegistryDownload, String> {
    let url = join_registry_url(
        registry.api_base_url(),
        &format!("/{}/{}/{version}/download", source.namespace, source.name),
    );
    let download: RegistryDownload = fetch_json(http, &url)?;
    Ok(download)
}

/// Get the global plugin cache directory.
///
/// Checks `CARINA_PLUGIN_CACHE_DIR` environment variable first,
/// then falls back to `~/.carina/plugin-cache/`.
/// Returns `None` if the home directory cannot be determined.
pub fn global_cache_dir() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("CARINA_PLUGIN_CACHE_DIR") {
        return Some(PathBuf::from(dir));
    }
    dirs::home_dir().map(|home| home.join(".carina").join("plugin-cache"))
}

/// Resolve the global cache path for a WASM provider.
fn global_cache_path_wasm(source: &str, version: &str) -> Option<PathBuf> {
    let repo = source.split('/').next_back().unwrap_or("provider");
    global_cache_dir().map(|dir| dir.join(source).join(version).join(format!("{repo}.wasm")))
}

/// Resolve the cache path for a provider binary.
pub fn cache_path(base_dir: &Path, source: &str, version: &str) -> PathBuf {
    let repo = source.split('/').next_back().unwrap_or("provider");
    base_dir
        .join(".carina")
        .join("providers")
        .join(source)
        .join(version)
        .join(repo)
}

/// Resolve the cache path for a WASM provider binary.
pub fn cache_path_wasm(base_dir: &Path, source: &str, version: &str) -> PathBuf {
    let repo = source.split('/').next_back().unwrap_or("provider");
    base_dir
        .join(".carina")
        .join("providers")
        .join(source)
        .join(version)
        .join(format!("{repo}.wasm"))
}

/// Compute SHA256 hex digest of a file.
pub fn sha256_file(path: &Path) -> io::Result<String> {
    Ok(sha256_digest_hex(&sha256_file_digest(path)?))
}

fn sha256_file_digest(path: &Path) -> io::Result<Sha256> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 8192];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
    }
    Ok(hasher)
}

fn sha256_digest_hex(digest: &Sha256) -> String {
    format!("{:x}", digest.clone().finalize())
}

fn hash_and_check(
    wasm_path: &Path,
    expected_shasum: &str,
    context: &str,
) -> Result<Sha256, String> {
    let artifact_digest = sha256_file_digest(wasm_path)
        .map_err(|error| format!("Failed to hash WASM binary: {error}"))?;
    let actual_hash = sha256_digest_hex(&artifact_digest);
    if actual_hash != expected_shasum {
        let _ = fs::remove_file(wasm_path);
        return Err(format!(
            "SHA256 mismatch for {context}. Expected registry shasum {expected_shasum}, got {actual_hash}. Re-run `carina init` to re-download."
        ));
    }
    Ok(artifact_digest)
}

/// Download a file from a URL and save it to a path.
fn download_to_file(url: &str, dest: &Path) -> Result<(), String> {
    let response = ureq::get(url)
        .call()
        .map_err(|e| format!("Failed to download {url}: {e}"))?;

    if response.status() != 200 {
        return Err(format!(
            "Download failed with status {}: {url}",
            response.status()
        ));
    }

    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create directory {}: {e}", parent.display()))?;
    }

    let mut reader = response.into_body().into_reader();
    let mut file = fs::File::create(dest)
        .map_err(|e| format!("Failed to create file {}: {e}", dest.display()))?;
    io::copy(&mut reader, &mut file)
        .map_err(|e| format!("Failed to write file {}: {e}", dest.display()))?;

    Ok(())
}

/// Extract a tar.gz archive. Returns the path to the extracted binary.
fn extract_tar_gz(archive_path: &Path, dest_dir: &Path) -> Result<PathBuf, String> {
    let file = fs::File::open(archive_path)
        .map_err(|e| format!("Failed to open archive {}: {e}", archive_path.display()))?;
    let decoder = flate2::read::GzDecoder::new(file);
    let mut archive = tar::Archive::new(decoder);

    fs::create_dir_all(dest_dir)
        .map_err(|e| format!("Failed to create dir {}: {e}", dest_dir.display()))?;

    archive
        .unpack(dest_dir)
        .map_err(|e| format!("Failed to extract archive: {e}"))?;

    // Find the binary in the extracted directory
    let entries = fs::read_dir(dest_dir)
        .map_err(|e| format!("Failed to read dir {}: {e}", dest_dir.display()))?;

    for entry in entries {
        let entry = entry.map_err(|e| format!("Failed to read entry: {e}"))?;
        let path = entry.path();
        if path.is_file()
            && !path
                .extension()
                .is_some_and(|ext| ext == "gz" || ext == "tar")
        {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mut perms = fs::metadata(&path)
                    .map_err(|e| format!("Failed to read metadata: {e}"))?
                    .permissions();
                perms.set_mode(0o755);
                fs::set_permissions(&path, perms)
                    .map_err(|e| format!("Failed to set permissions: {e}"))?;
            }
            return Ok(path);
        }
    }

    Err(format!(
        "No binary found in archive: {}",
        archive_path.display()
    ))
}

/// Validate a cached version-mode binary and ensure the lock file records it.
///
/// When a previous `carina init` left a binary in `.carina/providers/`, the
/// next run must still upsert a matching lock entry before the caller saves
/// the lock. Otherwise an empty in-memory `LockFile` gets written back to
/// disk and stomps the on-disk record (issue #2032).
fn verify_or_record_version_cache(
    binary_path: &Path,
    source: &str,
    version: &str,
    name: &str,
    lock_file: &mut LockFile,
) -> Result<(), String> {
    let actual_hash =
        sha256_file(binary_path).map_err(|e| format!("Failed to hash binary: {e}"))?;
    // Preserve any constraint already recorded; the resolver callers
    // overwrite it afterwards when the `.crn` specifies one.
    let existing_constraint = match lock_file.find(source, version) {
        Some(entry) => {
            if actual_hash != entry.sha256 {
                return Err(format!(
                    "SHA256 mismatch for provider '{}' ({}@{}). Expected: {}, got: {}. Re-run `carina init` to re-download.",
                    name, source, version, entry.sha256, actual_hash
                ));
            }
            match &entry.kind {
                LockEntryKind::Version { constraint, .. } => constraint.clone(),
                _ => None,
            }
        }
        None => None,
    };
    lock_file.upsert(LockEntry {
        name: name.to_string(),
        source: source.to_string(),
        kind: LockEntryKind::Version {
            version: version.to_string(),
            constraint: existing_constraint,
        },
        sha256: actual_hash,
        registry: None,
    });
    Ok(())
}

struct VerifiedRegistryLockPin {
    existing_constraint: Option<String>,
    expected_identity: Option<ExpectedIdentity>,
}

fn verify_registry_lock_pin(
    lock_file: &mut PersistentLockFile<'_>,
    source: &RegistrySource,
    version: &str,
    expected_shasum: &str,
    registry: &ResolvedRegistry,
    signature: Option<&RegistrySignature>,
    transparency_log_present: bool,
) -> Result<VerifiedRegistryLockPin, String> {
    let source_key = source.source_key();
    let current_lock = lock_file.lock_file();
    let entry = current_lock.find_by_source(&source_key);
    let existing_constraint = entry.and_then(|entry| match &entry.kind {
        LockEntryKind::Version { constraint, .. } => constraint.clone(),
        _ => None,
    });
    if let Some(entry) = entry
        && matches!(&entry.kind, LockEntryKind::Version { version: locked, .. } if locked == version)
        && entry.sha256 != expected_shasum
    {
        return Err(format!(
            "registry shasum pin mismatch for {}@{}: lock has {}, registry returned {}",
            source_key, version, entry.sha256, expected_shasum
        ));
    }
    let locked_registry = entry.and_then(|entry| entry.registry.as_ref());
    if let Some(locked_registry) = locked_registry
        && locked_registry.resolved_hostname() != registry.hostname
    {
        return Err(format!(
            "registry hostname pin mismatch for {}: lock has {}, resolved {}",
            source_key,
            locked_registry.resolved_hostname(),
            registry.hostname
        ));
    }
    let ratchets = current_lock
        .known_registry_ratchets(&source_key)
        .map_err(|error| error.to_string())?;
    let expected_identity = ratchets.signature.expected_identity();
    if ratchets.signature.is_required() && signature.is_none() {
        let remediation = recovery_remediation(IDENTITY_REPIN_REMEDIATION, &source_key);
        return Err(format!(
            "the resolved version of {source_key} has no registry signature, but carina-providers.lock records signatures as required for this provider; downgrades from signed to unsigned versions are refused and have no override. {remediation}"
        ));
    }
    if let (Some(expected_identity), Some(signature)) = (&expected_identity, signature) {
        let (certificate_identity, certificate_oidc_issuer) = expected_identity.values();
        if signature.certificate_identity != certificate_identity
            || signature.certificate_oidc_issuer != certificate_oidc_issuer
        {
            let remediation = recovery_remediation(IDENTITY_REPIN_REMEDIATION, &source_key);
            return Err(format!(
                "registry signature identity for {source_key} differs from the carina-providers.lock pin; signature verification has no override. {remediation}"
            ));
        }
    }
    if ratchets.transparency_log_present && !transparency_log_present {
        return Err(format!(
            "registry transparency_log field disappeared for {source_key}"
        ));
    }
    // Lock-pin validation is the presence observation's acceptance gate.
    // Persist it before later signature-type and artifact checks.
    lock_file.record_registry_transparency_log_presence(&source_key, transparency_log_present)?;
    Ok(VerifiedRegistryLockPin {
        existing_constraint,
        expected_identity,
    })
}

fn fetch_signature_bundle<H: RegistryHttp>(
    signature: &RegistrySignature,
    http: &H,
) -> Result<Vec<u8>, String> {
    if !signature.bundle_url.starts_with("https://") {
        return Err(signing::verification_failure(format!(
            "signature bundle URL must use HTTPS: {}",
            signature.bundle_url
        )));
    }
    let response = http
        .get(RegistryHttpRequest::Resource(&signature.bundle_url))
        .map_err(|error| {
        format!(
            "cannot fetch the signature bundle from {} ({error}); signature verification cannot proceed and has no override",
            signature.bundle_url
        )
    })?;
    let body = match response {
        HttpResponse::Success { body } => body,
        HttpResponse::Failure { status } => {
            return Err(format!(
                "cannot fetch the signature bundle from {} (HTTP {status}); signature verification cannot proceed and has no override",
                signature.bundle_url
            ));
        }
    };
    if body.len() > MAX_SIGNATURE_BUNDLE_BYTES {
        return Err(signing::verification_failure(format!(
            "signature bundle from {} is {} bytes, exceeding the {MAX_SIGNATURE_BUNDLE_BYTES}-byte limit",
            signature.bundle_url,
            body.len()
        )));
    }
    Ok(body)
}

fn resolve_registry_provider_with_http<H: RegistryHttp>(
    base_dir: &Path,
    source: &RegistrySource,
    version: &str,
    name: &str,
    lock_file: &mut LockFile,
    http: &H,
) -> Result<PathBuf, String> {
    let existing_host = lock_file.registry_host_lock(&source.hostname).cloned();
    let registry = resolve_registry(source, existing_host.as_ref(), http)?;
    let lock_path = base_dir.join("carina-providers.lock");
    let mut persistent_lock = PersistentLockFile::new(lock_file, lock_path);
    let RecordedRegistryListing {
        versions,
        validated_sequence,
    } = fetch_registry_versions(&registry, source, &mut persistent_lock, http)?;
    let source_key = source.source_key();
    if !versions
        .versions
        .iter()
        .any(|entry| entry.version == version)
    {
        return Err(format!(
            "Registry provider {} does not contain version {}",
            source_key, version
        ));
    }
    let listed_version_is_yanked = versions
        .versions
        .iter()
        .any(|entry| entry.version == version && entry.yanked);
    if listed_version_is_yanked {
        if persistent_lock
            .lock_file()
            .find(&source_key, version)
            .is_some()
        {
            eprintln!(
                "carina: warning: registry provider {source_key}@{version} is yanked; continuing because carina-providers.lock pins this version"
            );
        } else {
            return Err(format!(
                "Registry provider {source_key} version {version} is yanked and cannot be newly pinned"
            ));
        }
    }
    let download = fetch_registry_download(&registry, source, version, http)?;
    let transparency_log_present = download.transparency_log.is_some();
    let VerifiedRegistryLockPin {
        existing_constraint,
        expected_identity: pinned_identity,
    } = verify_registry_lock_pin(
        &mut persistent_lock,
        source,
        version,
        &download.shasum,
        &registry,
        download.signature.as_ref(),
        transparency_log_present,
    )?;
    // Transparency-log presence is an independently accepted promotion, so
    // the pin check records it before this downstream signature-type gate.
    if let Some(signature) = &download.signature {
        signing::ensure_supported_signature_type(&signature.r#type)?;
    }
    let provider_context = format!("registry provider '{name}' ({source_key}@{version})");
    let signed = download.signature.as_ref().map(|signature| {
        (
            signature,
            pinned_identity.unwrap_or_else(|| {
                ExpectedIdentity::first_use(
                    signature.certificate_identity.clone(),
                    signature.certificate_oidc_issuer.clone(),
                )
            }),
        )
    });

    let wasm_path = cache_path_wasm(base_dir, &source_key, version);
    let artifact_digest = if wasm_path.exists() {
        hash_and_check(
            &wasm_path,
            &download.shasum,
            &format!("cached {provider_context}"),
        )?
    } else {
        http.download_to_file(&download.download_url, &wasm_path)?;
        hash_and_check(&wasm_path, &download.shasum, &provider_context)?
    };

    let verified_identity = match signed {
        Some((signature, expected_identity)) => {
            let bundle = fetch_signature_bundle(signature, http)
                .map_err(|error| format!("{provider_context}: {error}"))?;
            match signing::verify(artifact_digest, &bundle, &expected_identity) {
                Ok(identity) => Some((identity, expected_identity)),
                Err(error) => {
                    let _ = fs::remove_file(&wasm_path);
                    return Err(format!("{provider_context}: {error}"));
                }
            }
        }
        None => None,
    };
    if let Some((identity, expected_identity)) = verified_identity {
        let (certificate_identity, certificate_oidc_issuer) = identity.into_parts();
        let first_use = expected_identity.is_first_use();
        let pin = IdentityPin {
            certificate_identity,
            certificate_oidc_issuer,
        };
        persistent_lock.record_verified_registry_signature(&source_key, pin.clone())?;
        if first_use {
            eprintln!(
                "carina: pinned signing identity for {source_key}: {} (issuer {})",
                pin.certificate_identity, pin.certificate_oidc_issuer
            );
        }
    }

    persistent_lock.upsert_registry_provider(RegistryProviderLockEntry {
        name: name.to_string(),
        source: source_key,
        kind: LockEntryKind::Version {
            version: version.to_string(),
            constraint: existing_constraint,
        },
        sha256: download.shasum,
        registry,
        validated_sequence,
    })?;
    Ok(wasm_path)
}

fn annotate_version_lock_entry(source: &str, config: &ProviderConfig, lock_file: &mut LockFile) {
    if let Some(entry) = lock_file.provider.iter_mut().find(|e| e.source == source)
        && let LockEntryKind::Version { version, .. } = &entry.kind
    {
        let version = version.clone();
        if let Some(revision) = &config.revision {
            entry.kind = LockEntryKind::RegistryRevision {
                revision: revision.clone(),
                version,
            };
        } else if let LockEntryKind::Version { constraint, .. } = &mut entry.kind {
            *constraint = config.version.as_ref().map(|c| c.raw.clone());
        }
    }
}

/// Resolve a single provider: download if missing, verify if cached.
///
/// Resolution order:
/// 1. Check WASM cache — use it if it exists (after SHA256 verification).
/// 2. Check native binary cache — use it if it exists (after SHA256 verification).
/// 3. Try downloading WASM first (platform-independent).
/// 4. Fall back to downloading the native binary as a tar.gz.
pub fn resolve_provider(
    base_dir: &Path,
    source: &str,
    version: &str,
    name: &str,
    lock_file: &mut LockFile,
) -> Result<PathBuf, String> {
    resolve_provider_with_http(
        base_dir,
        source,
        version,
        name,
        lock_file,
        &UreqRegistryHttp,
    )
}

fn resolve_provider_with_http<H: RegistryHttp>(
    base_dir: &Path,
    source: &str,
    version: &str,
    name: &str,
    lock_file: &mut LockFile,
    http: &H,
) -> Result<PathBuf, String> {
    if let ProviderSource::Registry(registry_source) = parse_provider_source(source)? {
        return resolve_registry_provider_with_http(
            base_dir,
            &registry_source,
            version,
            name,
            lock_file,
            http,
        );
    }

    // 1. Check local WASM cache first.
    let wasm_path = cache_path_wasm(base_dir, source, version);
    if wasm_path.exists() {
        verify_or_record_version_cache(&wasm_path, source, version, name, lock_file)?;
        return Ok(wasm_path);
    }

    // 2. Check native binary cache.
    let binary_path = cache_path(base_dir, source, version);
    if binary_path.exists() {
        verify_or_record_version_cache(&binary_path, source, version, name, lock_file)?;
        return Ok(binary_path);
    }

    // 3. Check global plugin cache for WASM.
    if let Some(global_wasm) = global_cache_path_wasm(source, version)
        && global_wasm.exists()
    {
        // Copy from global cache to local project
        if let Some(parent) = wasm_path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        fs::hard_link(&global_wasm, &wasm_path)
            .or_else(|_| fs::copy(&global_wasm, &wasm_path).map(|_| ()))
            .map_err(|e| format!("Failed to link/copy from global cache: {e}"))?;
        let hash =
            sha256_file(&wasm_path).map_err(|e| format!("Failed to hash WASM binary: {e}"))?;
        lock_file.upsert(LockEntry {
            name: name.to_string(),
            source: source.to_string(),
            kind: LockEntryKind::Version {
                version: version.to_string(),
                constraint: None,
            },
            sha256: hash,
            registry: None,
        });
        eprintln!(
            "Installed WASM provider '{}' from global cache ({}@{})",
            name, source, version
        );
        return Ok(wasm_path);
    }

    // 4. Try downloading WASM first (platform-independent).
    let wasm_url = download_url_wasm(source, version)?;
    eprintln!("Downloading WASM provider '{}' from {}", name, wasm_url);
    match download_to_file(&wasm_url, &wasm_path) {
        Ok(()) => {
            let hash =
                sha256_file(&wasm_path).map_err(|e| format!("Failed to hash WASM binary: {e}"))?;
            lock_file.upsert(LockEntry {
                name: name.to_string(),
                source: source.to_string(),
                kind: LockEntryKind::Version {
                    version: version.to_string(),
                    constraint: None,
                },
                sha256: hash,
                registry: None,
            });
            // Save to global cache
            if let Some(global_wasm) = global_cache_path_wasm(source, version) {
                if let Some(parent) = global_wasm.parent() {
                    let _ = fs::create_dir_all(parent);
                }
                let _ = fs::hard_link(&wasm_path, &global_wasm)
                    .or_else(|_| fs::copy(&wasm_path, &global_wasm).map(|_| ()));
            }
            eprintln!(
                "Installed WASM provider '{}' ({}@{})",
                name, source, version
            );
            return Ok(wasm_path);
        }
        Err(e) => {
            eprintln!(
                "WASM provider not available ({}), falling back to native binary: {}",
                wasm_url, e
            );
            // Clean up any partial download.
            let _ = fs::remove_file(&wasm_path);
        }
    }

    // 4. Fall back to downloading the native binary.
    let target = detect_target()?;
    let url = download_url(source, version, &target)?;

    eprintln!("Downloading provider '{}' from {}", name, url);

    let tmp_archive = base_dir
        .join(".carina")
        .join("providers")
        .join("tmp_download.tar.gz");

    download_to_file(&url, &tmp_archive)?;

    let dest_dir = binary_path.parent().unwrap();
    let extracted = extract_tar_gz(&tmp_archive, dest_dir)?;

    let _ = fs::remove_file(&tmp_archive);

    if extracted != binary_path {
        fs::rename(&extracted, &binary_path).map_err(|e| {
            format!(
                "Failed to rename {} to {}: {e}",
                extracted.display(),
                binary_path.display()
            )
        })?;
    }

    let hash = sha256_file(&binary_path).map_err(|e| format!("Failed to hash binary: {e}"))?;

    lock_file.upsert(LockEntry {
        name: name.to_string(),
        source: source.to_string(),
        kind: LockEntryKind::Version {
            version: version.to_string(),
            constraint: None,
        },
        sha256: hash,
        registry: None,
    });

    eprintln!("Installed provider '{}' ({}@{})", name, source, version);

    Ok(binary_path)
}

/// Resolve a single provider config with lock file management.
///
/// Handles version validation, lock file load/save, and delegation to `resolve_provider`.
pub fn resolve_single_config(base_dir: &Path, config: &ProviderConfig) -> Result<PathBuf, String> {
    resolve_single_config_with_http(base_dir, config, &UreqRegistryHttp)
}

fn resolve_single_config_with_http<H: RegistryHttp>(
    base_dir: &Path,
    config: &ProviderConfig,
    http: &H,
) -> Result<PathBuf, String> {
    let source = config
        .source
        .as_deref()
        .ok_or_else(|| format!("Provider '{}' has no source", config.name))?;
    let source = canonical_provider_source(source)?;

    let lock_path = base_dir.join("carina-providers.lock");
    let mut lock_file = LockFile::load(&lock_path)
        .map_err(|error| error.to_string())?
        .unwrap_or_default();

    let binary_path = if registry_revision(&source, config)?.is_some() {
        let version = resolve_version(&source, config, &mut lock_file, &lock_path, false, http)?;
        let path = resolve_provider_with_http(
            base_dir,
            &source,
            &version,
            &config.name,
            &mut lock_file,
            http,
        )?;
        annotate_version_lock_entry(&source, config, &mut lock_file);
        path
    } else if let Some(revision) = &config.revision {
        let (path, _sha) = crate::revision_resolver::resolve_provider_by_revision(
            base_dir,
            &source,
            revision,
            &config.name,
            &mut lock_file,
            false,
        )?;
        path
    } else {
        let version = resolve_version(&source, config, &mut lock_file, &lock_path, false, http)?;
        let path = resolve_provider_with_http(
            base_dir,
            &source,
            &version,
            &config.name,
            &mut lock_file,
            http,
        )?;

        annotate_version_lock_entry(&source, config, &mut lock_file);
        path
    };

    lock_file
        .save(&lock_path)
        .map_err(|e| format!("Failed to save carina-providers.lock: {e}"))?;

    Ok(binary_path)
}

/// Resolve a `file://` provider at its source path without copying it.
///
/// The LSP uses this direct-source view so its drift poll observes the same
/// file it loads, while still retaining typed file provenance.
pub fn find_file_provider_source(config: &ProviderConfig) -> Result<InstalledProvider, String> {
    let source = config
        .source
        .as_deref()
        .ok_or_else(|| format!("Provider '{}' has no source", config.name))?;
    let file_path = source
        .strip_prefix("file://")
        .ok_or_else(|| format!("Provider '{}' is not a file:// source", config.name))?;
    let path = PathBuf::from(file_path);
    if !path.exists() {
        return Err(format!("file not found: {file_path}"));
    }
    Ok(InstalledProvider {
        path,
        provider_name: config.name.clone(),
        provenance: ProviderArtifactProvenance::File {
            source: source.to_string(),
        },
    })
}

/// Find an already-installed provider without downloading.
///
/// Checks the project-local provider cache and lock file. Unlike a bare path,
/// the returned artifact always retains the lock/file provenance that selected
/// it. This is used by the LSP to avoid editor-triggered downloads as well as
/// by CLI load paths after `carina init` has installed the artifact.
pub fn find_installed_provider(
    base_dir: &Path,
    config: &ProviderConfig,
) -> Result<InstalledProvider, String> {
    let source = config
        .source
        .as_deref()
        .ok_or_else(|| format!("Provider '{}' has no source", config.name))?;

    // For file:// sources, look in .carina/providers/file/
    if let Some(file_path) = source.strip_prefix("file://") {
        let src = std::path::Path::new(file_path);
        let file_name = src
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("provider");
        let dest = base_dir
            .join(".carina")
            .join("providers")
            .join("file")
            .join(file_name)
            .join(
                src.file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("provider.wasm"),
            );
        if dest.exists() {
            return Ok(InstalledProvider {
                path: dest,
                provider_name: config.name.clone(),
                provenance: ProviderArtifactProvenance::File {
                    source: source.to_string(),
                },
            });
        }
        return Err(format!(
            "not installed. Run `carina init` in {}",
            base_dir.display()
        ));
    }
    let source = canonical_provider_source(source)?;

    let lock_path = base_dir.join("carina-providers.lock");
    let lock_file = LockFile::load(&lock_path)
        .map_err(|error| error.to_string())?
        .unwrap_or_default();

    // Only the project-local `.carina/` counts. The global plugin cache is an
    // install-time optimization consulted by `carina init`; treating it as a
    // runtime source lets validate/plan/apply silently succeed when a prior
    // project already pulled this provider and the current project has no
    // local install yet (issue #2018).
    if let Some(revision) = &config.revision {
        if registry_revision(&source, config)?.is_some() {
            if let Some(lock_entry) = lock_file.find_by_source(&source)
                && let LockEntryKind::RegistryRevision {
                    version,
                    revision: locked_revision,
                } = &lock_entry.kind
                && locked_revision == revision
            {
                let wasm_path = cache_path_wasm(base_dir, &source, version);
                let binary_path = cache_path(base_dir, &source, version);
                let path = if wasm_path.exists() {
                    Some(wasm_path)
                } else if binary_path.exists() {
                    Some(binary_path)
                } else {
                    None
                };
                let pin = LockedProviderPin::RegistryRevision {
                    revision: locked_revision.clone(),
                    version: version.clone(),
                };
                if let Some(path) = path {
                    return Ok(InstalledProvider {
                        path,
                        provider_name: config.name.clone(),
                        provenance: ProviderArtifactProvenance::LockFile {
                            lock_path: lock_path.clone(),
                            pin,
                        },
                    });
                }
                return Err(missing_locked_artifact_error(
                    base_dir,
                    &lock_path,
                    &pin,
                    Some(revision),
                ));
            }
        } else {
            if let Some(lock_entry) = lock_file.find_by_source(&source)
                && let LockEntryKind::Revision {
                    revision: locked_revision,
                    resolved_sha,
                } = &lock_entry.kind
            {
                let wasm_path =
                    crate::revision_resolver::cache_path_revision(base_dir, &source, resolved_sha);
                let pin = LockedProviderPin::Revision {
                    revision: locked_revision.clone(),
                    resolved_sha: resolved_sha.clone(),
                };
                if wasm_path.exists() {
                    return Ok(InstalledProvider {
                        path: wasm_path,
                        provider_name: config.name.clone(),
                        provenance: ProviderArtifactProvenance::LockFile {
                            lock_path: lock_path.clone(),
                            pin,
                        },
                    });
                }
                return Err(missing_locked_artifact_error(
                    base_dir,
                    &lock_path,
                    &pin,
                    Some(revision),
                ));
            }
        }
        return Err(format!(
            "not installed. Run `carina init` in {} to install (revision: {})",
            base_dir.display(),
            revision
        ));
    }

    if let Some(lock_entry) = lock_file.find_by_source(&source)
        && let LockEntryKind::Version {
            version,
            constraint,
        } = &lock_entry.kind
    {
        let wasm_path = cache_path_wasm(base_dir, &source, version);
        let binary_path = cache_path(base_dir, &source, version);
        let path = if wasm_path.exists() {
            Some(wasm_path)
        } else if binary_path.exists() {
            Some(binary_path)
        } else {
            None
        };
        let pin = LockedProviderPin::Version {
            version: version.clone(),
            constraint: constraint.clone(),
        };
        if let Some(path) = path {
            return Ok(InstalledProvider {
                path,
                provider_name: config.name.clone(),
                provenance: ProviderArtifactProvenance::LockFile { lock_path, pin },
            });
        }
        return Err(missing_locked_artifact_error(
            base_dir, &lock_path, &pin, None,
        ));
    }

    Err(format!(
        "not installed. Run `carina init` in {}",
        base_dir.display()
    ))
}

/// Returns true if the given path points to a WASM provider binary.
pub fn is_wasm_provider(path: &Path) -> bool {
    path.extension().is_some_and(|ext| ext == "wasm")
}

/// Decide whether the locked version can be reused for this version-mode config.
///
/// Returns `None` when the lock entry is missing, is not a version-mode entry,
/// or fails the configured constraint. The pattern match on `LockEntryKind`
/// means revision and file entries can't leak their stored strings into a
/// version-mode URL — the type rules out the #2028 failure mode at the call
/// site, no runtime check needed.
fn try_reuse_locked_version(
    source: &str,
    config: &ProviderConfig,
    lock_file: &LockFile,
) -> Option<String> {
    let entry = lock_file.find_by_source(source)?;
    if let Some(config_revision) = &config.revision {
        return match &entry.kind {
            LockEntryKind::RegistryRevision { revision, version }
                if revision == config_revision =>
            {
                Some(version.clone())
            }
            LockEntryKind::Version { .. }
            | LockEntryKind::Revision { .. }
            | LockEntryKind::RegistryRevision { .. }
            | LockEntryKind::File => None,
        };
    }

    match &entry.kind {
        LockEntryKind::Version {
            version,
            constraint: _,
        } => match &config.version {
            Some(constraint) if constraint.matches(version).unwrap_or(false) => {
                Some(version.clone())
            }
            None => Some(version.clone()),
            _ => None,
        },
        LockEntryKind::Revision { .. }
        | LockEntryKind::RegistryRevision { .. }
        | LockEntryKind::File => None,
    }
}

/// Resolve the exact version to use for a provider.
fn resolve_version<H: RegistryHttp>(
    source: &str,
    config: &ProviderConfig,
    lock_file: &mut LockFile,
    lock_path: &Path,
    upgrade: bool,
    http: &H,
) -> Result<String, String> {
    if !upgrade && let Some(version) = try_reuse_locked_version(source, config, lock_file) {
        return Ok(version);
    }

    if let ProviderSource::Registry(registry_source) = parse_provider_source(source)? {
        return resolve_registry_version_with_http(
            &registry_source,
            config,
            lock_file,
            lock_path,
            http,
        );
    }

    match &config.version {
        Some(constraint) => {
            let tags = crate::version_resolver::fetch_release_tags(source)?;
            let resolved = crate::version_resolver::resolve_from_tags(&tags, &constraint.req)
                .ok_or_else(|| {
                    format!(
                        "No release of '{}' matches constraint '{}'. Available: {}",
                        config.name,
                        constraint.raw,
                        tags.join(", ")
                    )
                })?;
            Ok(resolved.version.to_string())
        }
        None => {
            let tag = crate::version_resolver::fetch_latest_tag(source)?;
            let version = tag.strip_prefix('v').unwrap_or(&tag);
            Ok(version.to_string())
        }
    }
}

fn resolve_registry_version_with_http<H: RegistryHttp>(
    source: &RegistrySource,
    config: &ProviderConfig,
    lock_file: &mut LockFile,
    lock_path: &Path,
    http: &H,
) -> Result<String, String> {
    let existing_host = lock_file.registry_host_lock(&source.hostname).cloned();
    let registry = resolve_registry(source, existing_host.as_ref(), http)?;
    let RecordedRegistryListing { versions, .. } = {
        let mut persistent_lock = PersistentLockFile::new(lock_file, lock_path.to_path_buf());
        fetch_registry_versions(&registry, source, &mut persistent_lock, http)?
    };
    let candidates = SelectableRegistryVersions::from_listing(&versions.versions);
    select_registry_version(&candidates, config)
}

/// How strictly `resolve_all` treats a pre-existing lock file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockMode {
    /// Default for `carina init`: error on mismatch between `.crn` and lock,
    /// but a provider absent from the lock (first-time add) is accepted.
    Normal,
    /// Rebuild the lock from scratch: ignore existing entries and resolve
    /// every provider as if starting fresh. Set by `carina init --upgrade`.
    Upgrade,
    /// Strict CI mode: the lock must match the `.crn` exactly. A provider
    /// present in `.crn` but missing from the lock is an error.
    /// Set by `carina init --locked`. Mirrors Cargo's `--locked`.
    Locked,
}

/// Compare `.crn` provider configs against the lock file and return an
/// error when they disagree. Silent rewrites of the lock on mismatch were
/// defeating the reproducibility contract (issue #2026); every mature tool
/// (Cargo, npm ci, Terraform, Bundler) errors instead.
///
/// Categories detected:
/// - Version constraint that no longer accepts the locked version.
/// - `.crn` switched from version mode to revision mode (or vice versa)
///   since the lock was written.
/// - Same mode but different revision.
/// - (`--locked` only) provider present in `.crn` but missing from the lock.
///
/// Orphan lock entries (present in lock, absent in `.crn`) are intentionally
/// not reported here — they don't block `init` and the normal resolve loop
/// leaves them in place. `--upgrade` is the way to prune.
pub fn check_lock_mismatch(
    providers: &[ProviderConfig],
    lock_file: &LockFile,
    mode: LockMode,
) -> Result<(), String> {
    if mode == LockMode::Upgrade {
        return Ok(());
    }

    for config in providers {
        let source = match &config.source {
            Some(s) if !s.starts_with("file://") => s.as_str(),
            // No source or file:// — either the resolver skips it or the
            // sha256 is refreshed every run, so there's nothing to mismatch.
            _ => continue,
        };

        let lock_entry = match lock_file.find_by_source(source) {
            Some(entry) => entry,
            None => {
                if mode == LockMode::Locked {
                    return Err(format!(
                        "provider '{}' is declared in .crn but missing from carina-providers.lock\n\
                         hint: running with --locked requires the lock to be committed up-to-date;\n\
                               re-run without --locked (or `carina init --upgrade`) to populate it.",
                        config.name
                    ));
                }
                continue;
            }
        };

        match (&config.revision, &config.version, &lock_entry.kind) {
            // .crn has both revision and version (parser should reject this);
            // treat as accept and let the resolver surface its own error.
            (
                Some(_),
                Some(_),
                LockEntryKind::Version {
                    version: _,
                    constraint: _,
                },
            )
            | (
                Some(_),
                Some(_),
                LockEntryKind::Revision {
                    revision: _,
                    resolved_sha: _,
                },
            )
            | (
                Some(_),
                Some(_),
                LockEntryKind::RegistryRevision {
                    revision: _,
                    version: _,
                },
            )
            | (Some(_), Some(_), LockEntryKind::File) => {}
            // .crn revision — lock revision: must match literally.
            (
                Some(crn_rev),
                _,
                LockEntryKind::Revision {
                    revision: locked_rev,
                    resolved_sha: _,
                },
            ) => {
                if crn_rev != locked_rev {
                    return Err(mismatch_error(
                        &config.name,
                        &format!("revision = '{locked_rev}'"),
                        &format!("revision = '{crn_rev}'"),
                    ));
                }
            }
            // .crn revision — lock registry-revision: the locked branch must match.
            (
                Some(crn_rev),
                _,
                LockEntryKind::RegistryRevision {
                    revision: locked_rev,
                    version: _,
                },
            ) => {
                if crn_rev != locked_rev {
                    return Err(mismatch_error(
                        &config.name,
                        &format!("revision = '{locked_rev}'"),
                        &format!("revision = '{crn_rev}'"),
                    ));
                }
            }
            // .crn revision — lock plain version (mode switched).
            (
                Some(crn_rev),
                _,
                LockEntryKind::Version {
                    version: locked_ver,
                    constraint: _,
                },
            ) => {
                return Err(mismatch_error(
                    &config.name,
                    &format!("version  = '{locked_ver}'"),
                    &format!("revision = '{crn_rev}'"),
                ));
            }
            // .crn version constraint — lock version: constraint must still accept it.
            (
                None,
                Some(constraint),
                LockEntryKind::Version {
                    version: locked_ver,
                    constraint: _,
                },
            ) => {
                if !constraint.matches(locked_ver).unwrap_or(false) {
                    return Err(mismatch_error(
                        &config.name,
                        &format!("version = '{locked_ver}'"),
                        &format!("constraint = '{}'", constraint.raw),
                    ));
                }
            }
            // .crn version — lock revision (mode switched).
            (
                None,
                Some(constraint),
                LockEntryKind::Revision {
                    revision: locked_rev,
                    resolved_sha: _,
                },
            ) => {
                return Err(mismatch_error(
                    &config.name,
                    &format!("revision = '{locked_rev}'"),
                    &format!("version constraint = '{}'", constraint.raw),
                ));
            }
            // .crn version — lock registry revision (mode switched).
            (
                None,
                Some(constraint),
                LockEntryKind::RegistryRevision {
                    revision: locked_rev,
                    version: _,
                },
            ) => {
                return Err(mismatch_error(
                    &config.name,
                    &format!("revision = '{locked_rev}'"),
                    &format!("constraint = '{}'", constraint.raw),
                ));
            }
            // No constraint and no revision in .crn: the user didn't pin
            // anything explicitly. That implies version mode (latest tag).
            // Any pre-existing lock entry must also be version mode — a
            // revision-mode entry was written under a `.crn` that had
            // `revision = '...'` and is now gone, which is still a mismatch.
            (
                None,
                None,
                LockEntryKind::RegistryRevision {
                    revision: locked_rev,
                    version: _,
                },
            ) => {
                return Err(mismatch_error(
                    &config.name,
                    &format!("revision = '{locked_rev}'"),
                    "(no revision, no version constraint — version mode)",
                ));
            }
            (
                None,
                None,
                LockEntryKind::Version {
                    version: _,
                    constraint: _,
                },
            ) => {}
            (
                None,
                None,
                LockEntryKind::Revision {
                    revision: locked_rev,
                    resolved_sha: _,
                },
            ) => {
                return Err(mismatch_error(
                    &config.name,
                    &format!("revision = '{locked_rev}'"),
                    "(no revision, no version constraint — version mode)",
                ));
            }
            // .crn provider vs a file-mode lock entry: sources shouldn't match,
            // so this arm is effectively unreachable, but bail safely.
            (_, _, LockEntryKind::File) => {}
        }
    }

    Ok(())
}

fn mismatch_error(name: &str, lock_shape: &str, crn_shape: &str) -> String {
    format!(
        "lock file does not match providers.crn\n  \
         provider '{name}':\n    \
         providers.crn:  {crn_shape}\n    \
         lock:           {lock_shape}\n  \
         hint: run `carina init --upgrade` to resolve providers from the current\n        \
         configuration and rewrite carina-providers.lock"
    )
}

/// Resolve all providers that need GitHub source resolution.
pub fn resolve_all(
    base_dir: &Path,
    providers: &[ProviderConfig],
    mode: LockMode,
) -> Result<HashMap<String, PathBuf>, String> {
    resolve_all_with_http(base_dir, providers, mode, &UreqRegistryHttp)
}

fn resolve_all_with_http<H: RegistryHttp>(
    base_dir: &Path,
    providers: &[ProviderConfig],
    mode: LockMode,
    http: &H,
) -> Result<HashMap<String, PathBuf>, String> {
    let lock_path = base_dir.join("carina-providers.lock");
    let mut lock_file = LockFile::load(&lock_path)
        .map_err(|error| error.to_string())?
        .unwrap_or_default();

    // Fail before touching the filesystem if the lock disagrees with .crn.
    // Rewriting the lock requires `--upgrade`; `--locked` tightens this to
    // require every provider to be present in the lock too.
    check_lock_mismatch(providers, &lock_file, mode)?;

    let upgrade = mode == LockMode::Upgrade;
    let mut resolved = HashMap::new();

    for config in providers {
        let source = match &config.source {
            Some(s) => s.as_str(),
            _ => continue,
        };

        // Handle file:// sources: copy into .carina/providers/
        if let Some(file_path) = source.strip_prefix("file://") {
            let src_path = PathBuf::from(file_path);
            if !src_path.exists() {
                return Err(format!(
                    "Provider '{}': file source not found: {}",
                    config.name, file_path
                ));
            }
            let file_name = src_path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("provider");
            let dest = base_dir
                .join(".carina")
                .join("providers")
                .join("file")
                .join(file_name)
                .join(
                    src_path
                        .file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or("provider.wasm"),
                );
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent)
                    .map_err(|e| format!("Failed to create provider directory: {e}"))?;
            }
            // Remove existing file before hard-linking (hard_link fails if dest exists)
            let _ = fs::remove_file(&dest);
            fs::hard_link(&src_path, &dest)
                .map_err(|e| format!("Failed to link file:// provider: {e}"))?;
            let sha = sha256_file(&dest)
                .map_err(|e| format!("Failed to compute SHA256 for file:// provider: {e}"))?;

            // Update or add lock entry
            if let Some(entry) = lock_file.provider.iter_mut().find(|e| e.source == source) {
                entry.sha256 = sha;
            } else {
                lock_file.provider.push(LockEntry {
                    name: config.name.clone(),
                    source: source.to_string(),
                    kind: LockEntryKind::File,
                    sha256: sha,
                    registry: None,
                });
            }

            resolved.insert(config.name.clone(), dest);
            continue;
        }
        let source = canonical_provider_source(source)?;

        let binary_path = if registry_revision(&source, config)?.is_some() {
            let version =
                resolve_version(&source, config, &mut lock_file, &lock_path, upgrade, http)?;
            let path = resolve_provider_with_http(
                base_dir,
                &source,
                &version,
                &config.name,
                &mut lock_file,
                http,
            )?;
            annotate_version_lock_entry(&source, config, &mut lock_file);
            path
        } else if let Some(revision) = &config.revision {
            let (path, _sha) = crate::revision_resolver::resolve_provider_by_revision(
                base_dir,
                &source,
                revision,
                &config.name,
                &mut lock_file,
                upgrade,
            )?;
            path
        } else {
            let version =
                resolve_version(&source, config, &mut lock_file, &lock_path, upgrade, http)?;
            let path = resolve_provider_with_http(
                base_dir,
                &source,
                &version,
                &config.name,
                &mut lock_file,
                http,
            )?;

            annotate_version_lock_entry(&source, config, &mut lock_file);
            path
        };

        resolved.insert(config.name.clone(), binary_path);
    }

    if !resolved.is_empty() {
        lock_file
            .save(&lock_path)
            .map_err(|e| format!("Failed to save carina-providers.lock: {e}"))?;
    }

    Ok(resolved)
}

/// Validate that locked provider versions still satisfy the configured constraints.
///
/// Called before plan/apply to catch cases where the lock file and constraints have
/// drifted out of sync (e.g., the user tightened a constraint after last `carina init`).
pub fn validate_lock_constraints(
    base_dir: &Path,
    providers: &[ProviderConfig],
) -> Result<(), LockConstraintError> {
    let lock_path = base_dir.join("carina-providers.lock");
    let lock_file = match LockFile::load(&lock_path)? {
        Some(lf) => lf,
        None => return Ok(()),
    };

    for config in providers {
        // Skip revision-based providers — they don't use semver constraints
        if config.revision.is_some() {
            continue;
        }

        let source = match &config.source {
            Some(s) if !s.starts_with("file://") => s.as_str(),
            _ => continue,
        };

        let constraint = match &config.version {
            Some(c) => c,
            None => continue,
        };

        if let Some(lock_entry) = lock_file.find_by_source(source)
            && let LockEntryKind::Version { version, .. } = &lock_entry.kind
            && !constraint.matches(version).unwrap_or(false)
        {
            return Err(LockConstraintError::ConstraintMismatch {
                provider: config.name.clone(),
                locked_version: version.clone(),
                constraint: constraint.raw.clone(),
            });
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
    use indexmap::IndexMap;
    use std::io::Write;

    const SIGNED_FIXTURE_ARTIFACT: &[u8] = include_bytes!("signing/testdata/a.txt");
    const SIGNED_FIXTURE_BUNDLE: &[u8] = include_bytes!("signing/testdata/bundle.sigstore.json");
    const SIGNED_FIXTURE_IDENTITY: &str = "https://github.com/sigstore-conformance/extremely-dangerous-public-oidc-beacon/.github/workflows/extremely-dangerous-oidc-beacon.yml@refs/heads/main";
    const SIGNED_FIXTURE_ISSUER: &str = "https://token.actions.githubusercontent.com";
    const SIGNED_FIXTURE_BUNDLE_URL: &str = "https://downloads.example.test/aws.sigstore.json";
    const REGISTRY_VERSIONS_URL: &str =
        "https://registry.carina-rs.dev/v1/providers/carina-rs/aws/versions";
    const REGISTRY_DOWNLOAD_URL: &str =
        "https://registry.carina-rs.dev/v1/providers/carina-rs/aws/0.5.0/download";

    fn resolve_api_base_url_for_host(hostname: &str, providers_v1: &str) -> Result<String, String> {
        let discovery_url = registry_discovery_url(hostname)?;
        resolve_api_base_url(&discovery_url, providers_v1)
    }

    struct LockFileRenameHookGuard;

    impl Drop for LockFileRenameHookGuard {
        fn drop(&mut self) {
            LOCK_FILE_RENAME_HOOK.with(|hook| {
                hook.borrow_mut().take();
            });
        }
    }

    fn with_lock_file_rename_hook<T>(
        rename: impl FnOnce(&Path, &Path) -> io::Result<()> + 'static,
        operation: impl FnOnce() -> T,
    ) -> T {
        LOCK_FILE_RENAME_HOOK.with(|hook| {
            let mut hook = hook.borrow_mut();
            assert!(hook.is_none(), "lock-file rename hook is already installed");
            *hook = Some(Box::new(rename));
        });
        let _guard = LockFileRenameHookGuard;

        let result = operation();
        let hook_was_consumed = LOCK_FILE_RENAME_HOOK.with(|hook| hook.borrow().is_none());
        assert!(
            hook_was_consumed,
            "lock-file save returned before reaching the rename hook"
        );
        result
    }

    fn signature_pin(identity: &str, issuer: &str) -> RegistrySignatureProtection {
        RegistrySignatureProtection::RequiredPinned(IdentityPin {
            certificate_identity: identity.into(),
            certificate_oidc_issuer: issuer.into(),
        })
    }

    fn signed_fixture_pin() -> RegistrySignatureProtection {
        signature_pin(SIGNED_FIXTURE_IDENTITY, SIGNED_FIXTURE_ISSUER)
    }

    fn discovery_pin(api_base_url: &str) -> RegistryDiscoveryPin {
        RegistryDiscoveryPin {
            api_base_url: api_base_url.into(),
            additional: UnconsumedDiscoveryValues::default(),
        }
    }

    fn discovery_pin_with_unconsumed_values<const N: usize>(
        api_base_url: &str,
        values: [(&str, &str); N],
    ) -> RegistryDiscoveryPin {
        RegistryDiscoveryPin {
            api_base_url: api_base_url.into(),
            additional: UnconsumedDiscoveryValues::try_from_values(
                values
                    .into_iter()
                    .map(|(field, value)| (field.into(), value.into()))
                    .collect(),
            )
            .unwrap(),
        }
    }

    fn registry_host_lock(api_base_url: &str) -> RegistryHostLock {
        RegistryHostLock::pinned(discovery_pin(api_base_url))
    }

    fn registry_host_table(
        hostname: &str,
        api_base_url: &str,
    ) -> BTreeMap<String, RegistryHostLock> {
        BTreeMap::from([(hostname.into(), registry_host_lock(api_base_url))])
    }

    fn resolved_registry(hostname: &str, api_base_url: &str) -> ResolvedRegistry {
        ResolvedRegistry {
            hostname: hostname.into(),
            discovery_pin: discovery_pin(api_base_url),
        }
    }

    /// The lock-file shape from commit cd228086. In particular, this predates
    /// signing-identity pins and the top-level lock format version.
    #[derive(Serialize, Deserialize)]
    struct Cd228086LockFile {
        #[serde(default)]
        provider: Vec<Cd228086LockEntry>,
    }

    #[derive(Serialize, Deserialize)]
    struct Cd228086LockEntry {
        name: String,
        source: String,
        #[serde(flatten)]
        kind: Cd228086LockEntryKind,
        sha256: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        registry: Option<Cd228086RegistryLock>,
    }

    #[derive(Serialize, Deserialize)]
    #[serde(tag = "mode", rename_all = "lowercase")]
    enum Cd228086LockEntryKind {
        Version {
            version: String,
            #[serde(default, skip_serializing_if = "Option::is_none")]
            constraint: Option<String>,
        },
        Revision {
            revision: String,
            resolved_sha: String,
        },
        File,
    }

    #[derive(Serialize, Deserialize)]
    struct Cd228086RegistryLock {
        resolved_hostname: String,
        api_base_url: String,
        discovery_sha256: String,
        #[serde(default)]
        sequence_present: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sequence: Option<u64>,
        #[serde(default)]
        valid_until_present: bool,
        #[serde(default)]
        signature_present: bool,
        #[serde(default)]
        transparency_log_present: bool,
    }

    fn fully_protected_lock_toml() -> String {
        let mut lock = LockFile::default();
        lock.upsert_registry(
            LockEntry {
                name: "aws".into(),
                source: "carina-rs/aws".into(),
                kind: LockEntryKind::Version {
                    version: "0.5.0".into(),
                    constraint: None,
                },
                sha256: "abc".into(),
                registry: Some(RegistryLock {
                    resolved_hostname: "registry.carina-rs.dev".into(),
                    sequence: RegistrySequence::Present(7),
                    sequence_anchor: RegistrySequenceAnchor::Established(7),
                    valid_until_present: true,
                    yanked_versions: YankedRegistryVersions::default(),
                    signature: signed_fixture_pin(),
                    transparency_log_present: true,
                }),
            },
            registry_host_lock("https://registry.carina-rs.dev/v1/providers/"),
        )
        .unwrap();
        lock.to_toml_string().unwrap()
    }

    fn lock_with_registry_security_state(signature: RegistrySignatureProtection) -> LockFile {
        lock_with_registry_security_state_for_host("registry.carina-rs.dev", signature)
    }

    fn lock_with_registry_security_state_for_host(
        hostname: &str,
        signature: RegistrySignatureProtection,
    ) -> LockFile {
        let api_base_url = format!("https://{hostname}/v1/providers/");
        LockFile {
            version: LockFile::CURRENT_VERSION,
            registry_host: registry_host_table(hostname, &api_base_url),
            provider: vec![LockEntry {
                name: "aws".into(),
                source: "carina-rs/aws".into(),
                kind: LockEntryKind::Version {
                    version: "0.5.0".into(),
                    constraint: Some("^0.5".into()),
                },
                sha256: "pinned-shasum".into(),
                registry: Some(RegistryLock {
                    resolved_hostname: hostname.into(),
                    sequence: RegistrySequence::Present(7),
                    sequence_anchor: RegistrySequenceAnchor::Established(5),
                    valid_until_present: true,
                    yanked_versions: YankedRegistryVersions(BTreeSet::from(["0.4.0".into()])),
                    signature,
                    transparency_log_present: true,
                }),
            }],
            unpinned_registry_ratchets: UnpinnedRegistryRatchets::default(),
        }
    }

    fn lock_with_duplicate_registry_source(signature: RegistrySignatureProtection) -> LockFile {
        let mut lock = lock_with_registry_security_state(signature);
        let protected = lock.provider.pop().unwrap();
        lock.provider = vec![
            version_entry::<RegistryLock>("carina-rs/aws", "0.4.0"),
            protected,
        ];
        lock
    }

    fn assert_known_yank_is_still_refused(lock: &LockFile) {
        let source = match parse_provider_source("carina-rs/aws").unwrap() {
            ProviderSource::Registry(source) => source,
            ProviderSource::GithubDirect { .. } => unreachable!(),
        };
        let known = lock.known_registry_ratchets("carina-rs/aws").unwrap();
        assert!(
            known.yanked_versions.contains("0.4.0"),
            "the consumer-facing ratchets must retain the recorded yank"
        );
        let listing = RegistryVersions {
            sequence: Some(7),
            valid_until: Some("2999-01-01T00:00:00Z".into()),
            versions: vec![RegistryVersion {
                version: "0.4.0".into(),
                yanked: false,
            }],
        };
        let error = match ValidatedRegistryListing::validate(
            &source,
            &listing,
            &known,
            lock.registry_sequence_anchor("carina-rs/aws"),
        ) {
            Ok(_) => panic!("an explicitly recorded yank must not be reversible"),
            Err(error) => error,
        };
        assert!(error.contains("yanked flag disappeared"), "{error}");
        assert!(error.contains("0.4.0"), "{error}");
    }

    fn lock_toml_error(content: &str) -> LockFileError {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("carina-providers.lock");
        fs::write(&lock_path, content).unwrap();
        LockFile::load(&lock_path).expect_err("invalid protection metadata must be rejected")
    }

    fn assert_lock_toml_is_rejected(content: &str) {
        let error = lock_toml_error(content);
        assert!(matches!(error, LockFileError::Parse { .. }), "{error}");
    }

    fn version_entry<R>(source: &str, version: &str) -> LockEntry<R> {
        LockEntry {
            name: "awscc".into(),
            source: source.into(),
            kind: LockEntryKind::Version {
                version: version.into(),
                constraint: None,
            },
            sha256: "abc".into(),
            registry: None,
        }
    }

    fn revision_entry<R>(source: &str, revision: &str, sha: &str) -> LockEntry<R> {
        LockEntry {
            name: "awscc".into(),
            source: source.into(),
            kind: LockEntryKind::Revision {
                revision: revision.into(),
                resolved_sha: sha.into(),
            },
            sha256: "abc".into(),
            registry: None,
        }
    }

    fn provider_config(source: &str, revision: Option<&str>) -> ProviderConfig {
        ProviderConfig {
            name: "awscc".into(),
            source: Some(source.into()),
            version: None,
            revision: revision.map(|r| r.into()),
            attributes: IndexMap::new(),
            default_tags: IndexMap::new(),
            unresolved_attributes: IndexMap::new(),
            binding: None,
            is_default: true,
        }
    }

    #[test]
    fn registry_version_models_optional_yanked_flag() {
        let listing: RegistryVersions = serde_json::from_str(
            r#"{"versions":[{"version":"1.2.3","protocols":["1"]},{"version":"1.1.0","protocols":["1"],"yanked":false},{"version":"1.0.0","protocols":["1"],"yanked":true}]}"#,
        )
        .unwrap();

        assert!(!listing.versions[0].yanked);
        assert!(!listing.versions[1].yanked);
        assert!(listing.versions[2].yanked);
    }

    fn listed_version(version: &str, yanked: bool) -> RegistryVersion {
        RegistryVersion {
            version: version.into(),
            yanked,
        }
    }

    fn select_from_listing(
        versions: &[RegistryVersion],
        config: &ProviderConfig,
    ) -> Result<String, String> {
        let candidates = SelectableRegistryVersions::from_listing(versions);
        select_registry_version(&candidates, config)
    }

    #[test]
    fn registry_revision_selection_skips_yanked_versions() {
        let config = provider_config("carina-rs/aws", Some("main"));
        let versions = [
            listed_version("0.0.0-main.1.aaa", false),
            listed_version("0.0.0-main.10.bbb", true),
        ];

        assert_eq!(
            select_from_listing(&versions, &config).unwrap(),
            "0.0.0-main.1.aaa"
        );
    }

    #[test]
    fn registry_constraint_selection_skips_yanked_versions() {
        let config = versioned_config("carina-rs/aws", ">=1.0.0");
        let versions = [
            listed_version("1.0.0", false),
            listed_version("2.0.0", true),
        ];

        assert_eq!(select_from_listing(&versions, &config).unwrap(), "1.0.0");
    }

    #[test]
    fn registry_unconstrained_selection_skips_yanked_versions() {
        let config = provider_config("carina-rs/aws", None);
        let versions = [
            listed_version("1.0.0", false),
            listed_version("2.0.0", true),
        ];

        assert_eq!(select_from_listing(&versions, &config).unwrap(), "1.0.0");
    }

    #[test]
    fn registry_revision_error_names_skipped_yanked_versions() {
        let config = provider_config("carina-rs/aws", Some("main"));
        let versions = [
            listed_version("0.0.0-main.1.aaa", true),
            listed_version("0.0.0-main.10.bbb", true),
            listed_version("0.0.0-dev.20.ccc", false),
        ];

        let error = select_from_listing(&versions, &config).unwrap_err();

        assert!(error.contains("yanked"), "{error}");
        assert!(error.contains("0.0.0-main.1.aaa"), "{error}");
        assert!(error.contains("0.0.0-main.10.bbb"), "{error}");
    }

    #[test]
    fn registry_constraint_error_names_skipped_yanked_versions() {
        let config = versioned_config("carina-rs/aws", ">=1.0.0, <2.0.0");
        let versions = [
            listed_version("0.9.0", false),
            listed_version("1.1.0", true),
            listed_version("1.5.0", true),
            listed_version("2.0.0", true),
        ];

        let error = select_from_listing(&versions, &config).unwrap_err();

        assert!(error.contains("yanked"), "{error}");
        assert!(error.contains("1.1.0"), "{error}");
        assert!(error.contains("1.5.0"), "{error}");
        let skipped = error.split_once("skipped: ").unwrap().1;
        assert!(!skipped.contains("2.0.0"), "{error}");
    }

    #[test]
    fn registry_unconstrained_error_names_skipped_yanked_versions() {
        let config = provider_config("carina-rs/aws", None);
        let versions = [listed_version("1.0.0", true), listed_version("2.0.0", true)];

        let error = select_from_listing(&versions, &config).unwrap_err();

        assert!(error.contains("yanked"), "{error}");
        assert!(error.contains("1.0.0"), "{error}");
        assert!(error.contains("2.0.0"), "{error}");
    }

    #[test]
    fn registry_revision_selects_highest_matching_branch_prerelease() {
        let config = provider_config("carina-rs/aws", Some("main"));
        let versions = [
            RegistryVersion {
                version: "0.0.0-main.1.aaa".into(),
                yanked: false,
            },
            RegistryVersion {
                version: "0.0.0-main.10.bbb".into(),
                yanked: false,
            },
            RegistryVersion {
                version: "0.5.0".into(),
                yanked: false,
            },
            RegistryVersion {
                version: "0.0.0-dev.2.ccc".into(),
                yanked: false,
            },
        ];

        assert_eq!(
            select_from_listing(&versions, &config).unwrap(),
            "0.0.0-main.10.bbb"
        );

        let versions_with_malformed_high_precedence = [
            RegistryVersion {
                version: "0.0.0-main.5.aaa".into(),
                yanked: false,
            },
            RegistryVersion {
                version: "0.0.0-main.x".into(),
                yanked: false,
            },
        ];
        assert_eq!(
            select_from_listing(&versions_with_malformed_high_precedence, &config).unwrap(),
            "0.0.0-main.5.aaa"
        );

        let malformed_only = [RegistryVersion {
            version: "0.0.0-main.zzz".into(),
            yanked: false,
        }];
        let err = select_from_listing(&malformed_only, &config).unwrap_err();
        assert_eq!(
            err,
            "No registry version of 'awscc' matches revision 'main'"
        );

        let single_identifier = [RegistryVersion {
            version: "0.0.0-main".into(),
            yanked: false,
        }];
        let err = select_from_listing(&single_identifier, &config).unwrap_err();
        assert_eq!(
            err,
            "No registry version of 'awscc' matches revision 'main'"
        );

        let config = provider_config("carina-rs/aws", Some("feature"));
        let err = select_from_listing(&versions, &config).unwrap_err();
        assert_eq!(
            err,
            "No registry version of 'awscc' matches revision 'feature'"
        );
    }

    #[test]
    fn detect_target_returns_known_triple() {
        let target = detect_target().unwrap();
        assert!(
            target.contains("apple-darwin") || target.contains("unknown-linux"),
            "Unexpected target: {target}"
        );
    }

    #[test]
    fn download_url_builds_tarball_url() {
        let url = download_url(
            "github.com/carina-rs/carina-provider-awscc",
            "0.1.0",
            "aarch64-apple-darwin",
        )
        .unwrap();
        assert_eq!(
            url,
            "https://github.com/carina-rs/carina-provider-awscc/releases/download/v0.1.0/carina-provider-awscc-v0.1.0-aarch64-apple-darwin.tar.gz"
        );
    }

    #[test]
    fn download_url_wasm_builds_wasm_url() {
        let url = download_url_wasm("github.com/carina-rs/carina-provider-awscc", "0.1.0").unwrap();
        assert_eq!(
            url,
            "https://github.com/carina-rs/carina-provider-awscc/releases/download/v0.1.0/carina-provider-awscc-v0.1.0.wasm"
        );
    }

    #[test]
    fn download_url_rejects_invalid_source() {
        assert!(download_url("invalid-source", "0.1.0", "x86_64-unknown-linux-gnu").is_err());
        assert!(download_url_wasm("invalid-source", "0.1.0").is_err());
    }

    #[test]
    fn cache_path_lays_out_project_local_directory() {
        let base = Path::new("/tmp/project");
        let source = "github.com/carina-rs/carina-provider-awscc";
        assert_eq!(
            cache_path(base, source, "0.1.0"),
            PathBuf::from(
                "/tmp/project/.carina/providers/github.com/carina-rs/carina-provider-awscc/0.1.0/carina-provider-awscc"
            )
        );
        assert_eq!(
            cache_path_wasm(base, source, "0.1.0"),
            PathBuf::from(
                "/tmp/project/.carina/providers/github.com/carina-rs/carina-provider-awscc/0.1.0/carina-provider-awscc.wasm"
            )
        );
    }

    #[test]
    fn resolve_prefers_wasm_cache_over_native_binary() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        let source = "github.com/carina-rs/carina-provider-awscc";
        let version = "0.1.0";

        let wasm_path = cache_path_wasm(base, source, version);
        fs::create_dir_all(wasm_path.parent().unwrap()).unwrap();
        fs::File::create(&wasm_path)
            .unwrap()
            .write_all(b"fake wasm content")
            .unwrap();

        let native_path = cache_path(base, source, version);
        fs::File::create(&native_path)
            .unwrap()
            .write_all(b"fake native binary")
            .unwrap();

        let mut lock_file = LockFile::default();
        let result = resolve_provider(base, source, version, "awscc", &mut lock_file).unwrap();
        assert_eq!(result, wasm_path);
    }

    /// Issue #2032: when `resolve_provider` hits the project-local WASM cache,
    /// it must still upsert a lock entry before returning. Otherwise the caller
    /// writes an empty `LockFile` back to disk on subsequent `carina init` runs
    /// and silently wipes the existing entry.
    #[test]
    fn resolve_upserts_lock_entry_when_wasm_cache_is_hit() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        let source = "github.com/carina-rs/carina-provider-awscc";
        let version = "0.1.0";

        let wasm_path = cache_path_wasm(base, source, version);
        fs::create_dir_all(wasm_path.parent().unwrap()).unwrap();
        fs::File::create(&wasm_path)
            .unwrap()
            .write_all(b"fake wasm content")
            .unwrap();

        let mut lock_file = LockFile::default();
        resolve_provider(base, source, version, "awscc", &mut lock_file).unwrap();

        let entry = lock_file
            .find_by_source(source)
            .expect("cache-hit path must upsert a lock entry");
        match &entry.kind {
            LockEntryKind::Version {
                version: locked, ..
            } => assert_eq!(locked, version),
            other => panic!("expected Version variant, got {other:?}"),
        }
        assert!(!entry.sha256.is_empty(), "entry must record a sha256");
    }

    /// Same guarantee for the native binary cache path.
    #[test]
    fn resolve_upserts_lock_entry_when_native_cache_is_hit() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        let source = "github.com/carina-rs/carina-provider-awscc";
        let version = "0.1.0";

        // Only the native binary exists — no WASM in the cache.
        let native_path = cache_path(base, source, version);
        fs::create_dir_all(native_path.parent().unwrap()).unwrap();
        fs::File::create(&native_path)
            .unwrap()
            .write_all(b"fake native binary")
            .unwrap();

        let mut lock_file = LockFile::default();
        resolve_provider(base, source, version, "awscc", &mut lock_file).unwrap();

        assert!(
            lock_file.find_by_source(source).is_some(),
            "native-cache-hit path must upsert a lock entry"
        );
    }

    /// Round-trip a version-mode entry through TOML. The serialized form carries
    /// an explicit `mode = "version"` discriminator.
    #[test]
    fn version_mode_toml_roundtrip() {
        let source = "github.com/carina-rs/carina-provider-aws";
        let lock = LockFile {
            version: LockFile::CURRENT_VERSION,
            registry_host: BTreeMap::new(),
            provider: vec![LockEntry {
                name: "aws".into(),
                source: source.into(),
                kind: LockEntryKind::Version {
                    version: "0.5.2".into(),
                    constraint: Some("~0.5.0".into()),
                },
                sha256: "abc123".into(),
                registry: None,
            }],
            unpinned_registry_ratchets: UnpinnedRegistryRatchets::default(),
        };
        let toml_str = lock.to_toml_string().unwrap();
        assert!(
            toml_str.contains("mode = \"version\""),
            "serialized form should tag the variant: {toml_str}"
        );

        let loaded =
            LockFile::from_toml_str(&toml_str, Path::new("carina-providers.lock")).unwrap();
        assert_eq!(loaded.provider[0].kind, lock.provider[0].kind);
    }

    /// Registry-revision mode records both the originating branch and resolved
    /// published version under its own lock variant.
    #[test]
    fn registry_revision_mode_toml_roundtrip() {
        let lock = LockFile {
            version: LockFile::CURRENT_VERSION,
            registry_host: BTreeMap::new(),
            provider: vec![LockEntry {
                name: "aws".into(),
                source: "carina-rs/aws".into(),
                kind: LockEntryKind::RegistryRevision {
                    revision: "main".into(),
                    version: "0.0.0-main.10.bbb".into(),
                },
                sha256: "abc123".into(),
                registry: None,
            }],
            unpinned_registry_ratchets: UnpinnedRegistryRatchets::default(),
        };
        let toml_str = lock.to_toml_string().unwrap();
        assert!(
            toml_str.contains("mode = \"registryrevision\""),
            "serialized form should tag the variant: {toml_str}"
        );
        assert!(toml_str.contains("revision = \"main\""), "{toml_str}");
        assert!(
            toml_str.contains("version = \"0.0.0-main.10.bbb\""),
            "{toml_str}"
        );

        let loaded =
            LockFile::from_toml_str(&toml_str, Path::new("carina-providers.lock")).unwrap();
        assert_eq!(loaded.provider[0].kind, lock.provider[0].kind);
    }

    /// Revision-mode round-trip with the new tag. Note no `version` field.
    #[test]
    fn revision_mode_toml_roundtrip() {
        let lock = LockFile {
            version: LockFile::CURRENT_VERSION,
            registry_host: BTreeMap::new(),
            provider: vec![revision_entry(
                "github.com/carina-rs/carina-provider-awscc",
                "main",
                "81b6910fb34e84784daac2a02c915e821b2da570",
            )],
            unpinned_registry_ratchets: UnpinnedRegistryRatchets::default(),
        };
        let toml_str = lock.to_toml_string().unwrap();
        assert!(
            toml_str.contains("mode = \"revision\""),
            "serialized form should tag the variant: {toml_str}"
        );
        assert!(
            !toml_str.contains("version = \""),
            "revision-mode entry must not serialize a version field: {toml_str}"
        );

        let loaded =
            LockFile::from_toml_str(&toml_str, Path::new("carina-providers.lock")).unwrap();
        assert_eq!(loaded.provider[0].kind, lock.provider[0].kind);
    }

    #[test]
    fn file_mode_toml_roundtrip() {
        let lock = LockFile {
            version: LockFile::CURRENT_VERSION,
            registry_host: BTreeMap::new(),
            provider: vec![LockEntry {
                name: "test".into(),
                source: "file:///tmp/my-provider.wasm".into(),
                kind: LockEntryKind::File,
                sha256: "abc".into(),
                registry: None,
            }],
            unpinned_registry_ratchets: UnpinnedRegistryRatchets::default(),
        };
        let toml_str = lock.to_toml_string().unwrap();
        assert!(toml_str.contains("mode = \"file\""), "{toml_str}");

        let loaded =
            LockFile::from_toml_str(&toml_str, Path::new("carina-providers.lock")).unwrap();
        assert_eq!(loaded.provider[0].kind, LockEntryKind::File);
    }

    /// A lock file with an unknown or missing `mode` tag fails to parse instead
    /// of being silently accepted. That's the type-level replacement for the
    /// runtime validator removed with #2028's fix — there is no more flat shape
    /// the loader has to defend against.
    #[test]
    fn load_rejects_untagged_entry() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("carina-providers.lock");
        fs::write(
            &lock_path,
            r#"version = 3

[[provider]]
name = "awscc"
source = "github.com/carina-rs/carina-provider-awscc"
version = "0.5.2"
sha256 = "abc"
"#,
        )
        .unwrap();

        let err = LockFile::load(&lock_path)
            .expect_err("entry without a mode tag must not parse as any variant");
        let rendered = err.to_string();
        assert!(
            rendered.to_lowercase().contains("parse")
                || rendered.contains("carina init")
                || rendered.contains("mode"),
            "error should explain the parse failure: {err}"
        );
    }

    #[test]
    fn lock_load_rejects_set_shaped_unpinned_yanks() {
        let error = LockFile::from_toml_str(
            r#"version = 3

[unpinned_registry_yanks]
"carina-rs/aws" = ["0.4.0"]
"#,
            Path::new("carina-providers.lock"),
        )
        .expect_err("the removed set-shaped pending field must not be accepted");

        assert!(matches!(error, LockFileError::Parse { .. }), "{error}");
        let rendered = error.to_string();
        assert!(rendered.contains("unknown field"), "{rendered}");
        assert!(rendered.contains("unpinned_registry_yanks"), "{rendered}");
        assert!(
            rendered.contains("unpinned_registry_ratchets"),
            "{rendered}"
        );
    }

    #[test]
    fn resolve_parent_normalizes_empty_parent_to_current_directory() {
        assert_eq!(
            resolve_parent(Path::new("carina-providers.lock")),
            Path::new(".")
        );
        assert_eq!(
            resolve_parent(Path::new("locks/carina-providers.lock")),
            Path::new("locks")
        );
    }

    #[cfg(unix)]
    #[test]
    fn lock_file_save_creates_temporary_file_in_target_directory() {
        use std::os::unix::fs::MetadataExt;

        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("carina-providers.lock");
        let expected_parent = fs::canonicalize(dir.path()).unwrap();
        let hook_parent = expected_parent.clone();

        with_lock_file_rename_hook(
            move |temporary_path, target_path| {
                let temporary_parent = temporary_path
                    .parent()
                    .ok_or_else(|| io::Error::other("temporary file has no parent"))?;
                let temporary_parent = fs::canonicalize(temporary_parent)?;
                let target_parent = target_path
                    .parent()
                    .ok_or_else(|| io::Error::other("lock file has no parent"))?;
                let target_parent = fs::canonicalize(target_parent)?;

                if temporary_parent != hook_parent || target_parent != hook_parent {
                    return Err(io::Error::other(format!(
                        "temporary file parent {} and target parent {} must both be {}",
                        temporary_parent.display(),
                        target_parent.display(),
                        hook_parent.display()
                    )));
                }

                let temporary_device = fs::metadata(temporary_path)?.dev();
                let target_device = fs::metadata(&target_parent)?.dev();
                if temporary_device != target_device {
                    return Err(io::Error::other(format!(
                        "temporary file device {temporary_device} differs from target directory device {target_device}"
                    )));
                }

                fs::rename(temporary_path, target_path)
            },
            || LockFile::default().save(&lock_path),
        )
        .expect("save must stage its temporary file in the target directory");

        LockFile::load(&lock_path).unwrap().unwrap();
        assert_eq!(
            fs::canonicalize(lock_path.parent().unwrap()).unwrap(),
            expected_parent
        );
    }

    #[test]
    fn lock_file_save_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("carina-providers.lock");

        let mut lock = LockFile::default();
        lock.upsert(version_entry(
            "github.com/carina-rs/carina-provider-awscc",
            "0.1.0",
        ));

        lock.save(&lock_path).unwrap();
        let loaded = LockFile::load(&lock_path).unwrap().unwrap();
        assert_eq!(loaded.provider.len(), 1);
        assert_eq!(loaded.provider[0].name, "awscc");
    }

    #[test]
    fn lock_file_save_leaves_no_temporary_file() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("carina-providers.lock");

        LockFile::default().save(&lock_path).unwrap();

        let mut entries = fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        entries.sort();
        assert_eq!(
            entries,
            vec![std::ffi::OsString::from("carina-providers.lock")]
        );
    }

    #[test]
    fn lock_file_save_replaces_existing_contents_with_complete_document() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("carina-providers.lock");
        let old_content = LockFile::default().to_toml_string().unwrap();
        fs::write(&lock_path, old_content).unwrap();

        let mut new_lock = LockFile::default();
        new_lock.upsert(version_entry(
            "github.com/carina-rs/carina-provider-awscc",
            "0.2.0",
        ));
        let expected = new_lock.to_toml_string().unwrap();

        new_lock.save(&lock_path).unwrap();

        assert_eq!(fs::read_to_string(&lock_path).unwrap(), expected);
        let loaded = LockFile::load(&lock_path).unwrap().unwrap();
        assert_eq!(loaded.provider.len(), 1);
        assert_eq!(loaded.provider[0].kind.resolved_version(), Some("0.2.0"));
    }

    #[cfg(unix)]
    #[test]
    fn lock_file_save_new_file_uses_umask_adjusted_default_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let probe_path = dir.path().join("fs-write-mode-probe");
        let lock_path = dir.path().join("carina-providers.lock");

        // Observe the process umask through the same creation behavior as the
        // pre-atomic fs::write implementation instead of assuming 0o022.
        fs::write(&probe_path, b"").unwrap();
        let expected_mode = fs::metadata(&probe_path).unwrap().permissions().mode() & 0o777;

        LockFile::default().save(&lock_path).unwrap();

        let actual_mode = fs::metadata(&lock_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            actual_mode, expected_mode,
            "a newly created lock file must retain fs::write's umask-adjusted mode"
        );
    }

    #[cfg(unix)]
    #[test]
    fn lock_file_save_preserves_existing_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("carina-providers.lock");
        fs::write(&lock_path, b"old contents").unwrap();
        fs::set_permissions(&lock_path, fs::Permissions::from_mode(0o640)).unwrap();

        LockFile::default().save(&lock_path).unwrap();

        let actual_mode = fs::metadata(&lock_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            actual_mode, 0o640,
            "replacing a lock file must preserve its existing mode"
        );
    }

    // Atomic rename can replace a read-only file because replacement permission
    // belongs to its directory, unlike fs::write. This is inherent to atomic
    // replacement, and the target mode must remain preserved.
    #[cfg(unix)]
    #[test]
    fn lock_file_save_replaces_read_only_target_and_preserves_its_mode() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("carina-providers.lock");

        let mut original = LockFile::default();
        original.upsert(version_entry(
            "github.com/carina-rs/carina-provider-awscc",
            "0.1.0",
        ));
        original.save(&lock_path).unwrap();
        fs::set_permissions(&lock_path, fs::Permissions::from_mode(0o444)).unwrap();

        let mut replacement = LockFile::default();
        replacement.upsert(version_entry(
            "github.com/carina-rs/carina-provider-awscc",
            "0.2.0",
        ));
        let expected = replacement.to_toml_string().unwrap();

        replacement
            .save(&lock_path)
            .expect("atomic save must replace a read-only target");

        assert_eq!(fs::read_to_string(&lock_path).unwrap(), expected);
        let actual_mode = fs::metadata(&lock_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            actual_mode, 0o444,
            "replacing a read-only lock file must preserve its mode"
        );
        let loaded = LockFile::load(&lock_path).unwrap().unwrap();
        assert_eq!(loaded.provider.len(), 1);
        assert_eq!(loaded.provider[0].kind.resolved_version(), Some("0.2.0"));
    }

    #[cfg(unix)]
    #[test]
    fn lock_file_save_follows_existing_symlink() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let link_dir = dir.path().join("links");
        let target_dir = dir.path().join("targets");
        fs::create_dir(&link_dir).unwrap();
        fs::create_dir(&target_dir).unwrap();
        let link_path = link_dir.join("carina-providers.lock");
        let target_path = target_dir.join("carina-providers.lock");

        LockFile::default().save(&target_path).unwrap();
        symlink(&target_path, &link_path).unwrap();

        let mut replacement = LockFile::default();
        replacement.upsert(version_entry(
            "github.com/carina-rs/carina-provider-awscc",
            "0.2.0",
        ));
        let expected = replacement.to_toml_string().unwrap();
        replacement.save(&link_path).unwrap();

        assert!(
            fs::symlink_metadata(&link_path)
                .unwrap()
                .file_type()
                .is_symlink(),
            "save through a symlink must not replace the link itself"
        );
        assert_eq!(fs::read_to_string(&target_path).unwrap(), expected);
    }

    #[cfg(unix)]
    #[test]
    fn lock_file_save_follows_dangling_relative_symlink() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let link_dir = dir.path().join("links");
        let target_dir = dir.path().join("targets");
        fs::create_dir(&link_dir).unwrap();
        fs::create_dir(&target_dir).unwrap();
        let link_path = link_dir.join("carina-providers.lock");
        let target_path = target_dir.join("carina-providers.lock");
        symlink(Path::new("../targets/carina-providers.lock"), &link_path).unwrap();

        let mut lock = LockFile::default();
        lock.upsert(version_entry(
            "github.com/carina-rs/carina-provider-awscc",
            "0.2.0",
        ));
        let expected = lock.to_toml_string().unwrap();
        lock.save(&link_path).unwrap();

        assert!(
            fs::symlink_metadata(&link_path)
                .unwrap()
                .file_type()
                .is_symlink(),
            "save through a dangling symlink must not replace the link itself"
        );
        assert_eq!(fs::read_to_string(&target_path).unwrap(), expected);
        LockFile::load(&target_path).unwrap().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn lock_file_save_follows_dangling_multihop_relative_symlinks() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let link_dir = dir.path().join("links");
        let target_dir = dir.path().join("targets");
        fs::create_dir(&link_dir).unwrap();
        fs::create_dir(&target_dir).unwrap();
        let top_path = dir.path().join("top.lock");
        let middle_path = link_dir.join("mid.lock");
        let missing_path = target_dir.join("missing.lock");
        symlink(Path::new("links/mid.lock"), &top_path).unwrap();
        symlink(Path::new("../targets/missing.lock"), &middle_path).unwrap();
        assert!(!missing_path.exists());

        let mut lock = LockFile::default();
        lock.upsert(version_entry(
            "github.com/carina-rs/carina-provider-awscc",
            "0.2.0",
        ));
        let expected = lock.to_toml_string().unwrap();
        lock.save(&top_path).unwrap();

        assert!(
            fs::symlink_metadata(&top_path)
                .unwrap()
                .file_type()
                .is_symlink(),
            "save must preserve the top-level symlink"
        );
        assert!(
            fs::symlink_metadata(&middle_path)
                .unwrap()
                .file_type()
                .is_symlink(),
            "save must preserve every intermediate symlink"
        );
        assert_eq!(fs::read_to_string(&missing_path).unwrap(), expected);
        LockFile::load(&top_path).unwrap().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn lock_file_save_rejects_dangling_symlink_cycle_without_temporary_file() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let a_path = dir.path().join("a.lock");
        let b_path = dir.path().join("b.lock");
        symlink(Path::new("b.lock"), &a_path).unwrap();
        symlink(Path::new("a.lock"), &b_path).unwrap();
        let filesystem_loop_kind = fs::canonicalize(&a_path)
            .expect_err("the operating system must reject a symlink cycle")
            .kind();

        let error = LockFile::default()
            .save(&a_path)
            .expect_err("a dangling symlink cycle must not be followed forever");

        assert_eq!(error.kind(), filesystem_loop_kind);
        let mut entries = fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        entries.sort();
        assert_eq!(
            entries,
            vec![
                std::ffi::OsString::from("a.lock"),
                std::ffi::OsString::from("b.lock")
            ],
            "a rejected cycle must not leave a temporary file"
        );
    }

    #[test]
    fn lock_file_save_failure_preserves_original_complete_document() {
        const INJECTED_ERROR: &str = "injected lock-file rename failure";

        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("carina-providers.lock");

        let mut original = LockFile::default();
        original.upsert(version_entry(
            "github.com/carina-rs/carina-provider-awscc",
            "0.1.0",
        ));
        let original_content = original.to_toml_string().unwrap();
        fs::write(&lock_path, &original_content).unwrap();

        let mut replacement = LockFile::default();
        replacement.upsert(version_entry(
            "github.com/carina-rs/carina-provider-awscc",
            "0.2.0",
        ));
        let expected_temporary_content = replacement.to_toml_string().unwrap();
        let expected_target = lock_path.clone();

        let save_error = with_lock_file_rename_hook(
            move |temporary_path, target_path| {
                if target_path != expected_target {
                    return Err(io::Error::other(format!(
                        "rename target {} differs from expected {}",
                        target_path.display(),
                        expected_target.display()
                    )));
                }
                if fs::read_to_string(temporary_path)? != expected_temporary_content {
                    return Err(io::Error::other(
                        "temporary file did not contain the complete replacement document",
                    ));
                }
                Err(io::Error::other(INJECTED_ERROR))
            },
            || replacement.save(&lock_path),
        )
        .expect_err("the injected rename failure must make save fail");

        assert_eq!(save_error.kind(), io::ErrorKind::Other);
        assert_eq!(save_error.to_string(), INJECTED_ERROR);
        assert_eq!(fs::read_to_string(&lock_path).unwrap(), original_content);
        LockFile::load(&lock_path)
            .expect("the preserved lock file must remain parseable")
            .expect("the preserved lock file must still exist");

        let mut entries = fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        entries.sort();
        assert_eq!(
            entries,
            vec![std::ffi::OsString::from("carina-providers.lock")],
            "a failed save must not leave a temporary file"
        );
    }

    #[test]
    fn lock_file_save_accepts_parentless_relative_path() {
        const CHILD_ENV: &str = "CARINA_TEST_PARENTLESS_LOCK_SAVE";
        const TEST_NAME: &str =
            "provider_resolver::tests::lock_file_save_accepts_parentless_relative_path";

        let lock_path = Path::new("carina-providers.lock");
        let lock = LockFile::default();
        let expected = lock.to_toml_string().unwrap();

        if std::env::var_os(CHILD_ENV).is_some() {
            lock.save(lock_path).unwrap();
            assert_eq!(fs::read_to_string(lock_path).unwrap(), expected);
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([TEST_NAME, "--exact", "--nocapture"])
            .env(CHILD_ENV, "1")
            .current_dir(dir.path())
            .output()
            .unwrap();

        assert!(output.status.success(), "child test failed: {output:?}");
        assert_eq!(
            fs::read_to_string(dir.path().join(lock_path)).unwrap(),
            expected
        );
    }

    #[cfg(unix)]
    #[test]
    fn lock_file_save_replaces_target_inode() {
        use std::os::unix::fs::MetadataExt;

        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("carina-providers.lock");
        let old_content = LockFile::default().to_toml_string().unwrap();
        fs::write(&lock_path, old_content).unwrap();
        let old_inode = fs::metadata(&lock_path).unwrap().ino();

        let mut new_lock = LockFile::default();
        new_lock.upsert(version_entry(
            "github.com/carina-rs/carina-provider-awscc",
            "0.2.0",
        ));
        new_lock.save(&lock_path).unwrap();

        let new_inode = fs::metadata(&lock_path).unwrap().ino();
        assert_ne!(
            old_inode, new_inode,
            "save must replace the target instead of truncating it in place"
        );
        LockFile::load(&lock_path).unwrap().unwrap();
    }

    #[test]
    fn v3_host_pins_cannot_be_silently_read_by_cd228086() {
        let error = match toml::from_str::<Cd228086LockFile>(&fully_protected_lock_toml()) {
            Ok(_) => panic!("the old per-entry schema must not consume a v3 host-owned pin"),
            Err(error) => error,
        };

        assert!(error.to_string().contains("api_base_url"), "{error}");
    }

    #[test]
    fn lock_load_rejects_newer_format_with_typed_error() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("carina-providers.lock");
        let found = LockFile::CURRENT_VERSION + 1;
        fs::write(
            &lock_path,
            format!(
                "version = {found}\n\n[[provider]]\nmode = \"a-future-mode-current-carina-cannot-parse\"\n"
            ),
        )
        .unwrap();

        let error = LockFile::load(&lock_path).unwrap_err();
        assert!(matches!(
            error,
            LockFileError::VersionTooNew {
                found: actual,
                supported: LockFile::CURRENT_VERSION,
            } if actual == found
        ));
    }

    #[test]
    fn discovery_values_lock_format_is_v3() {
        assert_eq!(LockFile::CURRENT_VERSION, 3);
    }

    #[test]
    fn lock_load_rejects_v1_before_parsing_it_as_the_v3_schema() {
        let error = LockFile::from_toml_str(
            r#"version = 1

[[provider]]
name = "aws"
source = "carina-rs/aws"
mode = "version"
version = "0.5.0"
sha256 = "abc"

[provider.registry]
resolved_hostname = "registry.carina-rs.dev"
api_base_url = "https://registry.carina-rs.dev/v1/providers/"
discovery_sha256 = "def"
sequence_present = false
sequence_anchor_established = false
valid_until_present = false
signature_present = false
transparency_log_present = false
"#,
            Path::new("carina-providers.lock"),
        )
        .expect_err("v1 must be rejected before the v3 schema is parsed");

        assert!(matches!(
            &error,
            LockFileError::VersionTooOld {
                found: 1,
                supported: 3
            }
        ));
        let rendered = error.to_string();
        assert!(rendered.contains("version 1"), "{rendered}");
        assert!(rendered.contains("version 3"), "{rendered}");
        assert!(rendered.contains("cannot read"), "{rendered}");
        assert!(rendered.contains("regenerate"), "{rendered}");
        assert!(rendered.contains("`carina init`"), "{rendered}");
        assert!(
            rendered.contains("verifying registry discovery"),
            "{rendered}"
        );
        assert!(rendered.contains("providers afresh"), "{rendered}");
        assert!(rendered.contains("identity pins"), "{rendered}");
        assert!(rendered.contains("first contact"), "{rendered}");
        assert!(!rendered.contains("unknown field"), "{rendered}");
    }

    #[test]
    fn lock_load_rejects_registry_entry_whose_host_record_is_absent() {
        let error = LockFile::from_toml_str(
            r#"version = 3

[[provider]]
name = "aws"
source = "carina-rs/aws"
mode = "version"
version = "0.5.0"
sha256 = "abc"

[provider.registry]
resolved_hostname = "registry.carina-rs.dev"
sequence_present = false
sequence_anchor_established = false
valid_until_present = false
signature_present = false
transparency_log_present = false
"#,
            Path::new("carina-providers.lock"),
        )
        .expect_err("a registry entry must not outlive its host record");

        assert!(matches!(
            &error,
            LockFileError::MissingRegistryHostRecord {
                provider,
                hostname,
                ..
            } if provider == "carina-rs/aws" && hostname == "registry.carina-rs.dev"
        ));
        let rendered = error.to_string();
        assert!(rendered.contains("registry.carina-rs.dev"), "{rendered}");
        assert!(rendered.contains("host record"), "{rendered}");
        assert!(rendered.contains("must be restored"), "{rendered}");
        assert!(rendered.contains("`carina init`"), "{rendered}");
        assert!(
            rendered.contains("re-resolve against that host"),
            "{rendered}"
        );
        assert!(
            rendered.contains("re-establish the discovery pin"),
            "{rendered}"
        );
        assert!(
            !rendered.to_ascii_lowercase().contains("delete"),
            "{rendered}"
        );
    }

    #[test]
    fn lock_load_rejects_v2_before_parsing_it_as_the_v3_schema() {
        let error = LockFile::from_toml_str(
            r#"version = 2

[registry_host."registry.carina-rs.dev"]
discovery_pin_present = true
api_base_url = "https://registry.carina-rs.dev/v1/providers/"
discovery_sha256 = "host-pin"

[[provider]]
name = "aws"
source = "carina-rs/aws"
mode = "version"
version = "0.5.0"
sha256 = "abc"

[provider.registry]
resolved_hostname = "registry.carina-rs.dev"
api_base_url = "https://registry.carina-rs.dev/v1/providers/"
discovery_sha256 = "provider-a-pin"
sequence_present = false
sequence_anchor_established = false
valid_until_present = false
signature_present = false
transparency_log_present = false

"#,
            Path::new("carina-providers.lock"),
        )
        .expect_err("v2 must be rejected before the v3 values-map schema is parsed");

        assert!(matches!(
            &error,
            LockFileError::VersionTooOld {
                found: 2,
                supported: 3
            }
        ));
        let rendered = error.to_string();
        assert!(rendered.contains("version 2"), "{rendered}");
        assert!(rendered.contains("version 3"), "{rendered}");
        assert!(rendered.contains("cannot read"), "{rendered}");
        assert!(rendered.contains("regenerate"), "{rendered}");
        assert!(!rendered.contains("unknown field"), "{rendered}");
    }

    #[test]
    fn one_registry_host_pin_round_trips_for_multiple_provider_entries() {
        let serialized = r#"version = 3

[registry_host."registry.carina-rs.dev"]
discovery_pin_present = true

[registry_host."registry.carina-rs.dev".discovery_values]
"providers.v1" = "https://registry.carina-rs.dev/v1/providers/"

[[provider]]
name = "aws"
source = "carina-rs/aws"
mode = "version"
version = "0.5.0"
sha256 = "abc"

[provider.registry]
resolved_hostname = "registry.carina-rs.dev"
sequence_present = false
sequence_anchor_established = false
valid_until_present = false
signature_present = false
transparency_log_present = false

[[provider]]
name = "random"
source = "carina-rs/random"
mode = "version"
version = "1.0.0"
sha256 = "xyz"

[provider.registry]
resolved_hostname = "registry.carina-rs.dev"
sequence_present = false
sequence_anchor_established = false
valid_until_present = false
signature_present = false
transparency_log_present = false
"#;

        let lock = LockFile::from_toml_str(serialized, Path::new("carina-providers.lock"))
            .expect("both entries should reference the one host-owned discovery pin");
        assert_eq!(lock.provider.len(), 2);
        assert_eq!(lock.registry_host.len(), 1);
        assert_eq!(
            lock.registry_host
                .get("registry.carina-rs.dev")
                .and_then(RegistryHostLock::pin),
            Some(&discovery_pin(
                "https://registry.carina-rs.dev/v1/providers/"
            ))
        );
        let round_tripped = lock.to_toml_string().unwrap();
        assert_eq!(round_tripped.matches("\"providers.v1\" =").count(), 1);
        assert!(!round_tripped.contains("discovery_sha256"));
        assert_eq!(
            LockFile::from_toml_str(&round_tripped, Path::new("carina-providers.lock"))
                .map(|lock| lock.to_toml_string().unwrap())
                .unwrap(),
            round_tripped,
            "the host table must serialize deterministically"
        );
    }

    #[test]
    fn unconsumed_discovery_pin_values_round_trip_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("carina-providers.lock");
        let serialized = format!(
            r#"version = {}

[registry_host."registry.carina-rs.dev"]
discovery_pin_present = true

[registry_host."registry.carina-rs.dev".discovery_values]
"modules.v1" = "https://registry.carina-rs.dev/v1/modules/"
"providers.v1" = "https://registry.carina-rs.dev/v1/providers/"
"#,
            LockFile::CURRENT_VERSION
        );
        fs::write(&lock_path, serialized).unwrap();

        let lock = LockFile::load(&lock_path)
            .expect("a values-map host pin must load")
            .expect("the lock fixture must exist");
        lock.save(&lock_path).unwrap();
        let saved = fs::read_to_string(&lock_path).unwrap();

        assert!(
            saved.contains("\"modules.v1\" = \"https://registry.carina-rs.dev/v1/modules/\""),
            "{saved}"
        );
        assert_eq!(saved.matches("\"modules.v1\" =").count(), 1, "{saved}");
        LockFile::load(&lock_path)
            .expect("the round-tripped lock must remain parseable")
            .expect("the round-tripped lock must remain present");
    }

    #[test]
    fn registry_discovery_pin_direct_deserialization_requires_providers_v1() {
        #[derive(Debug, Deserialize)]
        struct DirectPinFixture {
            #[serde(rename = "discovery_values")]
            _discovery_values: RegistryDiscoveryPin,
        }

        let error = toml::from_str::<DirectPinFixture>(
            r#"[discovery_values]
"modules.v1" = "https://registry.carina-rs.dev/v1/modules/"
"#,
        )
        .expect_err("RegistryDiscoveryPin itself must reject a missing providers.v1 value");

        assert!(error.to_string().contains("providers.v1"), "{error}");
    }

    #[test]
    fn registry_discovery_pin_direct_deserialization_rejects_invalid_api_base_shape() {
        for api_base_url in ["http://evil.test/", "#fragment", "?query=only"] {
            let serialized = format!(r#""providers.v1" = "{api_base_url}""#);
            let error = toml::from_str::<RegistryDiscoveryPin>(&serialized)
                .expect_err("a persisted providers.v1 value must be an absolute HTTPS URL");

            assert!(error.to_string().contains("absolute HTTPS URL"), "{error}");
        }
    }

    #[test]
    fn unconsumed_discovery_values_cannot_carry_providers_v1() {
        let error = UnconsumedDiscoveryValues::try_from_values(BTreeMap::from([(
            PROVIDERS_V1_DISCOVERY_FIELD.into(),
            "https://registry.carina-rs.dev/v1/providers/".into(),
        )]))
        .expect_err("the retained-values type must reject consumed discovery material");

        assert!(error.to_string().contains("providers.v1"), "{error}");
    }

    #[test]
    fn registry_host_lock_error_preserves_invalid_discovery_pin_source() {
        let error = RegistryHostLockError::InvalidDiscoveryPin(
            RegistryDiscoveryPinError::MissingProvidersV1,
        );
        let source = std::error::Error::source(&error)
            .expect("the invalid discovery pin must remain in the error chain");

        assert_eq!(
            source.to_string(),
            "registry discovery values are missing required providers.v1"
        );
    }

    #[test]
    fn discovery_pin_comparison_ignores_retained_unconsumed_values() {
        let locked = discovery_pin_with_unconsumed_values(
            "https://registry.carina-rs.dev/v1/providers/",
            [("modules.v1", "https://registry.carina-rs.dev/v1/modules/")],
        );
        let resolved = discovery_pin_with_unconsumed_values(
            "https://registry.carina-rs.dev/v1/providers/",
            [("modules.v1", "https://registry.carina-rs.dev/v2/modules/")],
        );

        assert!(same_consumed_discovery_values(&locked, &resolved));
    }

    #[test]
    fn mutating_one_host_pin_is_observed_by_every_provider_for_that_host() {
        let hostname = "registry.carina-rs.dev";
        let mut lock = LockFile::default();
        lock.registry_host.insert(
            hostname.into(),
            RegistryHostLock::pinned(discovery_pin(
                "https://registry.carina-rs.dev/v1/providers/",
            )),
        );
        let providers = [
            ("aws", "carina-rs/aws", "0.5.0", "aws-shasum"),
            ("random", "carina-rs/random", "1.0.0", "random-shasum"),
        ];
        lock.provider = providers
            .iter()
            .copied()
            .map(|(name, source, version, sha256)| LockEntry {
                name: name.into(),
                source: source.into(),
                kind: LockEntryKind::Version {
                    version: version.into(),
                    constraint: None,
                },
                sha256: sha256.into(),
                registry: Some(RegistryLock {
                    resolved_hostname: hostname.into(),
                    sequence: RegistrySequence::Absent,
                    sequence_anchor: RegistrySequenceAnchor::Unestablished,
                    valid_until_present: false,
                    yanked_versions: YankedRegistryVersions::default(),
                    signature: RegistrySignatureProtection::NotRequired,
                    transparency_log_present: false,
                }),
            })
            .collect();
        let resolved = resolved_registry(hostname, "https://registry.carina-rs.dev/v1/providers/");
        let dir = tempfile::tempdir().unwrap();

        assert_eq!(lock.registry_host.len(), 1);
        for (_, source, version, shasum) in providers {
            let source = match parse_provider_source(source).unwrap() {
                ProviderSource::Registry(source) => source,
                ProviderSource::GithubDirect { .. } => unreachable!(),
            };
            let mut persistent =
                PersistentLockFile::new(&mut lock, dir.path().join("carina-providers.lock"));
            verify_registry_lock_pin(
                &mut persistent,
                &source,
                version,
                shasum,
                &resolved,
                None,
                true,
            )
            .unwrap_or_else(|error| {
                panic!(
                    "the original host pin must verify for {}: {error}",
                    source.source_key()
                )
            });
        }

        let host_lock = lock
            .registry_host
            .get_mut(hostname)
            .expect("the shared host pin must still exist");
        let RegistryDiscoveryPinState::Pinned(pin) = &mut host_lock.discovery else {
            panic!("the shared host pin must still be pinned");
        };
        pin.api_base_url = "https://EVIL.example.com/v1/providers/".into();

        for (_, source, _, _) in providers {
            let source = match parse_provider_source(source).unwrap() {
                ProviderSource::Registry(source) => source,
                ProviderSource::GithubDirect { .. } => unreachable!(),
            };
            let error = match resolve_registry(
                &source,
                lock.registry_host.get(hostname),
                &FakeRegistryHttp::default().json(
                    "https://registry.carina-rs.dev/.well-known/carina.json",
                    r#"{"providers.v1":"/v1/providers/"}"#,
                ),
            ) {
                Ok(_) => panic!("the one mutated host pin must reject every referencing provider"),
                Err(error) => error,
            };

            assert!(
                error.contains(
                    "registry pinned discovery values mismatch for host registry.carina-rs.dev"
                ),
                "{error}"
            );
            assert!(
                error.contains("carina providers repin-discovery registry.carina-rs.dev"),
                "{error}"
            );
            assert!(!error.contains(&source.source_key()), "{error}");
        }
    }

    #[test]
    fn hostname_mismatch_does_not_report_the_discovery_repin_operation() {
        let mut lock = lock_with_registry_security_state_for_host(
            "old.registry.example",
            RegistrySignatureProtection::NotRequired,
        );
        let source = match parse_provider_source("carina-rs/aws").unwrap() {
            ProviderSource::Registry(source) => source,
            ProviderSource::GithubDirect { .. } => unreachable!(),
        };
        let resolved = resolved_registry(
            "registry.carina-rs.dev",
            "https://registry.carina-rs.dev/v1/providers/",
        );
        let mut persistent =
            PersistentLockFile::new(&mut lock, PathBuf::from("carina-providers.lock"));
        let error = match verify_registry_lock_pin(
            &mut persistent,
            &source,
            "0.5.0",
            "pinned-shasum",
            &resolved,
            None,
            false,
        ) {
            Ok(_) => panic!("a provider hostname pin must reject a changed resolved hostname"),
            Err(error) => error,
        };
        assert!(error.contains("registry hostname pin mismatch"), "{error}");
        assert!(
            error.contains("lock has old.registry.example, resolved registry.carina-rs.dev"),
            "{error}"
        );
        assert!(
            !error.contains("carina providers repin-discovery"),
            "{error}"
        );
    }

    #[test]
    fn discovery_values_mismatch_reports_the_host_and_repin_operation() {
        let source = match parse_provider_source("carina-rs/aws").unwrap() {
            ProviderSource::Registry(source) => source,
            ProviderSource::GithubDirect { .. } => unreachable!(),
        };
        let locked_pin = discovery_pin_with_unconsumed_values(
            "https://registry.carina-rs.dev/v1/providers/",
            [("modules.v1", "opaque-retained-value-from-a-newer-client")],
        );
        let locked_host = RegistryHostLock::pinned(locked_pin);
        let http = FakeRegistryHttp::default().json(
            "https://registry.carina-rs.dev/.well-known/carina.json",
            r#"{"providers.v1":"/v2/providers/"}"#,
        );
        let error = resolve_registry(&source, Some(&locked_host), &http)
            .expect_err("changed pinned discovery values must trip the host pin");

        assert!(
            error.contains(
                "registry pinned discovery values mismatch for host registry.carina-rs.dev"
            ),
            "{error}"
        );
        assert!(
            error.contains(
                "pinned providers.v1 was https://registry.carina-rs.dev/v1/providers/; resolved providers.v1 is https://registry.carina-rs.dev/v2/providers/"
            ),
            "{error}"
        );
        assert!(
            error.contains("carina providers repin-discovery registry.carina-rs.dev"),
            "{error}"
        );
        assert!(
            !error.contains("opaque-retained-value-from-a-newer-client"),
            "{error}"
        );
        assert!(!error.contains("carina-rs/aws"), "{error}");
    }

    #[test]
    fn lock_load_rejects_unversioned_infra_lock_with_typed_remediation() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("carina-providers.lock");
        fs::write(
            &lock_path,
            r#"[[provider]]
name = "awscc"
source = "github.com/carina-rs/carina-provider-awscc"
mode = "revision"
revision = "main"
resolved_sha = "967e645a7153522ca60ef942183d3fc338fc7c27"
sha256 = "3bd19254ba60717dabdc12c663ef96e0be72e5a2fbc192cf3a5d15ef6578f14f"
"#,
        )
        .unwrap();

        let error = LockFile::load(&lock_path).unwrap_err();
        assert!(matches!(
            &error,
            LockFileError::MissingVersion { path } if path == &lock_path
        ));
        let rendered = error.to_string();
        assert!(
            rendered.contains("predates lock format versioning"),
            "{rendered}"
        );
        assert!(
            rendered.contains("protection fields were stripped"),
            "{rendered}"
        );
        assert!(
            rendered.contains("Delete it, then regenerate it with `carina init`"),
            "{rendered}"
        );
    }

    #[test]
    fn lock_load_keeps_malformed_toml_as_parse_error() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("carina-providers.lock");
        fs::write(&lock_path, "version = [\n").unwrap();

        let error = LockFile::load(&lock_path).unwrap_err();
        assert!(matches!(error, LockFileError::Parse { .. }), "{error}");
    }

    #[test]
    fn lock_load_accepts_signature_required_while_awaiting_identity_pin() {
        let stripped = fully_protected_lock_toml()
            .lines()
            .filter(|line| {
                !line.starts_with("certificate_identity")
                    && !line.starts_with("certificate_oidc_issuer")
            })
            .collect::<Vec<_>>()
            .join("\n");
        let lock = LockFile::from_toml_str(&stripped, Path::new("carina-providers.lock"))
            .expect("required-but-unpinned signature state must be representable");
        assert_eq!(
            lock.known_registry_ratchets("carina-rs/aws")
                .unwrap()
                .signature,
            RegistrySignatureProtection::RequiredUnpinned
        );
    }

    #[test]
    fn lock_load_rejects_identity_when_signature_is_not_required() {
        let contradictory = fully_protected_lock_toml()
            .replace("signature_present = true\n", "signature_present = false\n");
        assert_lock_toml_is_rejected(&contradictory);
    }

    #[test]
    fn lock_load_rejects_partial_signature_identity_pin() {
        let contradictory = fully_protected_lock_toml()
            .lines()
            .filter(|line| !line.starts_with("certificate_oidc_issuer"))
            .collect::<Vec<_>>()
            .join("\n");
        assert_lock_toml_is_rejected(&contradictory);
    }

    #[test]
    fn lock_load_rejects_stripped_signature_presence() {
        let stripped = fully_protected_lock_toml().replace("signature_present = true\n", "");
        assert_lock_toml_is_rejected(&stripped);
    }

    #[test]
    fn lock_load_rejects_stripped_sequence_protection() {
        for stripped in [
            fully_protected_lock_toml().replace("sequence = 7\n", ""),
            fully_protected_lock_toml().replace("sequence_present = true\n", ""),
            fully_protected_lock_toml().replace("sequence_anchor_established = true\n", ""),
            fully_protected_lock_toml().replace("sequence_anchor = 7\n", ""),
            fully_protected_lock_toml().replace("sequence_anchor = 7\n", "sequence_anchor = 8\n"),
        ] {
            assert_lock_toml_is_rejected(&stripped);
        }
    }

    #[test]
    fn lock_load_rejects_stripped_transparency_log_presence() {
        let stripped = fully_protected_lock_toml().replace("transparency_log_present = true\n", "");
        assert_lock_toml_is_rejected(&stripped);
    }

    #[test]
    fn registry_host_discovery_pin_is_all_or_nothing_on_load() {
        let pinned = fully_protected_lock_toml();
        for inconsistent in [
            pinned.replace(
                "\n[registry_host.\"registry.carina-rs.dev\".discovery_values]\n\"providers.v1\" = \"https://registry.carina-rs.dev/v1/providers/\"\n",
                "",
            ),
            pinned.replace(
                "\"providers.v1\" = \"https://registry.carina-rs.dev/v1/providers/\"\n",
                "",
            ),
            pinned.replace("discovery_pin_present = true\n", ""),
            pinned.replace(
                "discovery_pin_present = true\n",
                "discovery_pin_present = false\n",
            ),
        ] {
            assert_lock_toml_is_rejected(&inconsistent);
        }
    }

    #[test]
    fn registry_host_load_rejects_unpinned_state_carrying_providers_v1() {
        let inconsistent = fully_protected_lock_toml().replace(
            "discovery_pin_present = true\n",
            "discovery_pin_present = false\n",
        );

        let error = LockFile::from_toml_str(&inconsistent, Path::new("carina-providers.lock"))
            .expect_err("an unpinned host must not retain the consumed providers.v1 value");

        assert!(error.to_string().contains("providers.v1"), "{error}");
    }

    #[test]
    fn existing_registry_locks_default_sticky_yanked_set_to_empty() {
        let serialized = fully_protected_lock_toml();
        assert!(serialized.starts_with("version = 3\n"), "{serialized}");
        assert!(!serialized.contains("yanked_versions"), "{serialized}");

        let lock =
            LockFile::from_toml_str(&serialized, Path::new("carina-providers.lock")).unwrap();
        let registry = lock.provider[0].registry.as_ref().unwrap();
        assert!(registry.yanked_versions().is_empty());
        assert!(
            lock.to_toml_string().unwrap().starts_with("version = 3\n"),
            "the defaulted yank set must remain stable within lock format v3"
        );
    }

    #[test]
    fn registry_identity_pin_toml_roundtrip_preserves_flat_bytes() {
        let lock = LockFile {
            version: LockFile::CURRENT_VERSION,
            registry_host: registry_host_table(
                "registry.carina-rs.dev",
                "https://registry.carina-rs.dev/v1/providers/",
            ),
            provider: vec![LockEntry {
                name: "aws".into(),
                source: "carina-rs/aws".into(),
                kind: LockEntryKind::Version {
                    version: "0.5.0".into(),
                    constraint: None,
                },
                sha256: "abc".into(),
                registry: Some(RegistryLock {
                    resolved_hostname: "registry.carina-rs.dev".into(),
                    sequence: RegistrySequence::Present(7),
                    sequence_anchor: RegistrySequenceAnchor::Established(7),
                    valid_until_present: true,
                    yanked_versions: YankedRegistryVersions::default(),
                    signature: RegistrySignatureProtection::RequiredPinned(IdentityPin {
                        certificate_identity: SIGNED_FIXTURE_IDENTITY.into(),
                        certificate_oidc_issuer: SIGNED_FIXTURE_ISSUER.into(),
                    }),
                    transparency_log_present: false,
                }),
            }],
            unpinned_registry_ratchets: UnpinnedRegistryRatchets::default(),
        };
        let expected = format!(
            r#"version = 3

[registry_host."registry.carina-rs.dev"]
discovery_pin_present = true

[registry_host."registry.carina-rs.dev".discovery_values]
"providers.v1" = "https://registry.carina-rs.dev/v1/providers/"

[[provider]]
name = "aws"
source = "carina-rs/aws"
mode = "version"
version = "0.5.0"
sha256 = "abc"

[provider.registry]
resolved_hostname = "registry.carina-rs.dev"
sequence_present = true
sequence = 7
sequence_anchor_established = true
sequence_anchor = 7
valid_until_present = true
signature_present = true
certificate_identity = {SIGNED_FIXTURE_IDENTITY:?}
certificate_oidc_issuer = {SIGNED_FIXTURE_ISSUER:?}
transparency_log_present = false
"#
        );

        let serialized = lock.to_toml_string().unwrap();
        assert_eq!(serialized, expected);
        let reparsed =
            LockFile::from_toml_str(&serialized, Path::new("carina-providers.lock")).unwrap();
        assert_eq!(reparsed.to_toml_string().unwrap(), expected);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("carina-providers.lock");
        reparsed.save(&path).unwrap();
        let saved = fs::read(path).unwrap();
        assert_eq!(saved.as_slice(), expected.as_bytes());
    }

    #[test]
    fn registry_signature_states_roundtrip_through_lock_and_ratchet_serializers() {
        let states = [
            RegistrySignatureProtection::NotRequired,
            RegistrySignatureProtection::RequiredUnpinned,
            signature_pin("identity-a", "issuer-a"),
        ];

        for signature in states {
            let lock = lock_with_registry_security_state(signature.clone());
            let serialized_lock = lock.to_toml_string().unwrap();
            let reparsed_lock =
                LockFile::from_toml_str(&serialized_lock, Path::new("carina-providers.lock"))
                    .unwrap();
            assert_eq!(
                reparsed_lock
                    .known_registry_ratchets("carina-rs/aws")
                    .unwrap()
                    .signature,
                signature
            );

            let ratchets = RegistryRatchets {
                signature: signature.clone(),
                ..RegistryRatchets::default()
            };
            let serialized_ratchets = toml::to_string(&ratchets).unwrap();
            let reparsed_ratchets: RegistryRatchets = toml::from_str(&serialized_ratchets).unwrap();
            assert_eq!(reparsed_ratchets.signature, signature);
        }
    }

    #[test]
    fn registry_ratchet_serializer_rejects_contradictory_signature_encodings() {
        for encoded in [
            RegistryRatchetsSerde {
                sequence_present: false,
                sequence: None,
                valid_until_present: false,
                yanked_versions: BTreeSet::new(),
                signature_present: false,
                certificate_identity: Some("identity-a".into()),
                certificate_oidc_issuer: Some("issuer-a".into()),
                transparency_log_present: false,
            },
            RegistryRatchetsSerde {
                sequence_present: false,
                sequence: None,
                valid_until_present: false,
                yanked_versions: BTreeSet::new(),
                signature_present: true,
                certificate_identity: Some("identity-a".into()),
                certificate_oidc_issuer: None,
                transparency_log_present: false,
            },
        ] {
            assert!(matches!(
                RegistryRatchets::try_from(encoded),
                Err(RegistryLockError::InconsistentSignature)
            ));
        }
    }

    #[test]
    fn registry_signature_merge_retains_requirement_and_any_existing_pin() {
        let awaiting = RegistryRatchets {
            signature: RegistrySignatureProtection::RequiredUnpinned,
            ..RegistryRatchets::default()
        };
        let pinned = RegistryRatchets {
            signature: signature_pin("identity-a", "issuer-a"),
            ..RegistryRatchets::default()
        };

        assert_eq!(
            RegistryRatchets::default()
                .merge(&awaiting)
                .unwrap()
                .signature,
            RegistrySignatureProtection::RequiredUnpinned
        );
        assert_eq!(
            awaiting.clone().merge(&pinned).unwrap().signature,
            signature_pin("identity-a", "issuer-a")
        );
        assert_eq!(
            pinned.merge(&awaiting).unwrap().signature,
            signature_pin("identity-a", "issuer-a")
        );
    }

    #[test]
    fn normal_registry_ratchet_storage_cannot_lower_signature_requirement() {
        let source = "carina-rs/aws";
        let downgrade = RegistryRatchets::default();

        let mut pinned =
            lock_with_registry_security_state(RegistrySignatureProtection::RequiredUnpinned);
        pinned
            .store_registry_ratchets(source, downgrade.clone())
            .unwrap();
        assert!(
            pinned.provider[0]
                .registry
                .as_ref()
                .unwrap()
                .signature
                .is_required()
        );

        let mut unpinned = LockFile::default();
        unpinned
            .store_registry_ratchets(
                source,
                RegistryRatchets {
                    signature: RegistrySignatureProtection::RequiredUnpinned,
                    ..RegistryRatchets::default()
                },
            )
            .unwrap();
        unpinned.store_registry_ratchets(source, downgrade).unwrap();
        assert!(
            unpinned
                .known_registry_ratchets(source)
                .unwrap()
                .signature
                .is_required()
        );
    }

    #[test]
    fn rebootstrap_uses_one_duplicate_entry_for_preview_and_commit() {
        let mut lock = lock_with_duplicate_registry_source(signature_pin("identity-a", "issuer-a"));
        let before = lock.provider[1].registry.as_ref().unwrap().clone();

        let recovery = lock.prepare_registry_rebootstrap("carina-rs/aws").unwrap();
        let preview = recovery.freshness();
        assert_eq!(preview.sequence, before.sequence.value());
        assert_eq!(preview.sequence_anchor, before.sequence_anchor.value());
        recovery.commit().unwrap();

        assert!(lock.provider[0].registry.is_none());
        let after = lock.provider[1].registry.as_ref().unwrap();
        assert_eq!(after.sequence, RegistrySequence::Absent);
        assert_eq!(after.sequence_anchor, RegistrySequenceAnchor::Unestablished);
        assert_eq!(after.yanked_versions, before.yanked_versions);
        assert_eq!(after.signature, before.signature);
        assert_eq!(after.valid_until_present, before.valid_until_present);
        assert_eq!(
            after.transparency_log_present,
            before.transparency_log_present
        );
    }

    #[test]
    fn repin_identity_uses_one_duplicate_entry_for_preview_and_commit() {
        let mut lock = lock_with_duplicate_registry_source(signature_pin("identity-a", "issuer-a"));
        let before = lock.provider[1].registry.as_ref().unwrap().clone();

        let recovery = lock
            .prepare_registry_identity_repin("carina-rs/aws")
            .unwrap();
        assert_eq!(
            recovery.identity_pin(),
            before.signature.identity_pin().unwrap()
        );
        recovery.commit().unwrap();

        assert!(lock.provider[0].registry.is_none());
        let after = lock.provider[1].registry.as_ref().unwrap();
        assert_eq!(
            after.signature,
            RegistrySignatureProtection::RequiredUnpinned
        );
        assert_eq!(after.sequence, before.sequence);
        assert_eq!(after.sequence_anchor, before.sequence_anchor);
        assert_eq!(after.yanked_versions, before.yanked_versions);
        assert_eq!(after.valid_until_present, before.valid_until_present);
        assert_eq!(
            after.transparency_log_present,
            before.transparency_log_present
        );
    }

    #[test]
    fn repin_identity_preserves_yank_and_all_other_registry_ratchets() {
        let mut lock = lock_with_registry_security_state(signature_pin("identity-a", "issuer-a"));
        let before_entry = lock.find_by_source("carina-rs/aws").unwrap().clone();

        let recovery = lock
            .prepare_registry_identity_repin("registry.carina-rs.dev/carina-rs/aws")
            .unwrap();
        let preview = recovery.identity_pin().clone();
        assert_eq!(preview.certificate_identity, "identity-a");
        recovery.commit().unwrap();

        let ratchets = lock.known_registry_ratchets("carina-rs/aws").unwrap();
        assert_known_yank_is_still_refused(&lock);
        assert_eq!(ratchets.sequence, RegistrySequence::Present(7));
        assert!(ratchets.valid_until_present);
        assert!(ratchets.transparency_log_present);
        assert_eq!(
            ratchets.signature,
            RegistrySignatureProtection::RequiredUnpinned
        );
        assert_eq!(
            lock.registry_sequence_anchor("carina-rs/aws"),
            RegistrySequenceAnchor::Established(5)
        );
        let after_entry = lock.find_by_source("carina-rs/aws").unwrap();
        assert_eq!(after_entry.sha256, before_entry.sha256);
        assert_eq!(after_entry.kind, before_entry.kind);
    }

    #[test]
    fn prepared_discovery_repin_preview_is_total_by_construction() {
        let mut lock = lock_with_registry_security_state(RegistrySignatureProtection::NotRequired);
        let prepared = lock
            .prepare_registry_discovery_repin("registry.carina-rs.dev")
            .unwrap();

        // This in-module assertion pins the structural guarantee directly:
        // every prepared value carries a non-optional pin, and the public
        // preview accessor is only a projection of that field.
        assert!(std::ptr::eq(
            prepared.discovery_pin(),
            &prepared.discarded_pin
        ));
    }

    #[test]
    fn discovery_repin_normalizes_mixed_case_host_argument() {
        let mut lock = lock_with_registry_security_state(RegistrySignatureProtection::NotRequired);

        lock.prepare_registry_discovery_repin("Registry.Carina-RS.dev")
            .unwrap()
            .commit();

        assert!(
            lock.registry_host
                .get("registry.carina-rs.dev")
                .unwrap()
                .pin()
                .is_none()
        );
    }

    #[test]
    fn discovery_repin_clears_consumed_host_values_and_preserves_every_provider_field() {
        let hostname = "registry.carina-rs.dev";
        let mut lock = lock_with_registry_security_state(signature_pin("identity-a", "issuer-a"));
        lock.provider.push(LockEntry {
            name: "random".into(),
            source: "carina-rs/random".into(),
            kind: LockEntryKind::Version {
                version: "1.0.0".into(),
                constraint: Some("^1".into()),
            },
            sha256: "random-shasum".into(),
            registry: Some(RegistryLock {
                resolved_hostname: hostname.into(),
                sequence: RegistrySequence::Present(11),
                sequence_anchor: RegistrySequenceAnchor::Established(9),
                valid_until_present: true,
                yanked_versions: YankedRegistryVersions(BTreeSet::from(["0.9.0".into()])),
                signature: signature_pin("identity-b", "issuer-b"),
                transparency_log_present: true,
            }),
        });
        lock.registry_host.insert(
            "registry.example.test".into(),
            registry_host_lock("https://registry.example.test/providers/"),
        );
        let providers_before = serde_json::to_value(&lock.provider).unwrap();
        let other_host_before = lock
            .registry_host
            .get("registry.example.test")
            .unwrap()
            .clone();

        let recovery = lock.prepare_registry_discovery_repin(hostname).unwrap();
        assert_eq!(
            recovery.discovery_pin(),
            &discovery_pin("https://registry.carina-rs.dev/v1/providers/")
        );
        recovery.commit();

        assert!(
            lock.registry_host
                .get(hostname)
                .expect("the host record itself must survive")
                .pin()
                .is_none(),
            "the next verified discovery fetch must see first contact"
        );
        assert_eq!(
            lock.registry_host.get("registry.example.test"),
            Some(&other_host_before),
            "other hosts must not be changed"
        );
        assert_eq!(
            serde_json::to_value(&lock.provider).unwrap(),
            providers_before,
            "discovery recovery must not rewrite any provider field"
        );
        assert!(
            lock.provider[0]
                .registry
                .as_ref()
                .unwrap()
                .yanked_versions
                .contains("0.4.0")
        );
        assert!(
            lock.provider[1]
                .registry
                .as_ref()
                .unwrap()
                .yanked_versions
                .contains("0.9.0")
        );
    }

    #[test]
    fn discovery_repin_preserves_unconsumed_pinned_values() {
        let hostname = "registry.carina-rs.dev";
        let mut lock = lock_with_registry_security_state(RegistrySignatureProtection::NotRequired);
        let host = lock.registry_host.get_mut(hostname).unwrap();
        let RegistryDiscoveryPinState::Pinned(pin) = &mut host.discovery else {
            panic!("fixture host must begin pinned");
        };
        pin.additional = UnconsumedDiscoveryValues::try_from_values(BTreeMap::from([(
            "modules.v1".into(),
            "https://registry.carina-rs.dev/v1/modules/".into(),
        )]))
        .unwrap();

        lock.prepare_registry_discovery_repin(hostname)
            .unwrap()
            .commit();
        let serialized = lock.to_toml_string().unwrap();

        assert!(
            serialized.contains("\"modules.v1\" = \"https://registry.carina-rs.dev/v1/modules/\""),
            "{serialized}"
        );
        assert!(!serialized.contains("\"providers.v1\" ="), "{serialized}");
        assert!(
            serialized.contains("discovery_pin_present = false"),
            "{serialized}"
        );
        let reloaded =
            LockFile::from_toml_str(&serialized, Path::new("carina-providers.lock")).unwrap();
        let reloaded_host = reloaded.registry_host.get(hostname).unwrap();
        assert!(
            reloaded_host.pin().is_none(),
            "retained unconsumed values must not re-arm the consumed API-base pin"
        );

        let source = match parse_provider_source("carina-rs/aws").unwrap() {
            ProviderSource::Registry(source) => source,
            ProviderSource::GithubDirect { .. } => unreachable!(),
        };
        let resolved = resolve_registry(
            &source,
            Some(reloaded_host),
            &FakeRegistryHttp::default().json(
                "https://registry.carina-rs.dev/.well-known/carina.json",
                r#"{"providers.v1":"/v1/providers/"}"#,
            ),
        )
        .expect("first contact must re-pin the consumed value");
        assert_eq!(
            resolved
                .discovery_pin
                .values()
                .find(|(field, _)| *field == "modules.v1")
                .map(|(_, value)| value),
            Some("https://registry.carina-rs.dev/v1/modules/")
        );
        assert_eq!(
            resolved.discovery_pin.api_base_url(),
            "https://registry.carina-rs.dev/v1/providers/"
        );
    }

    #[test]
    fn discovery_repin_supplies_no_replacement_and_round_trips_unpinned() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("carina-providers.lock");
        let mut lock = lock_with_registry_security_state(signature_pin("identity-a", "issuer-a"));

        let missing_error = match lock.prepare_registry_discovery_repin("missing.example.test") {
            Ok(_) => panic!("an unknown host must not produce a prepared recovery"),
            Err(error) => error,
        };
        assert_eq!(
            missing_error,
            RegistryLockRecoveryError::RegistryHostStateNotFound {
                host: "missing.example.test".into(),
            }
        );

        lock.prepare_registry_discovery_repin("registry.carina-rs.dev")
            .unwrap()
            .commit();
        lock.save(&lock_path).unwrap();
        let mut reloaded = LockFile::load(&lock_path).unwrap().unwrap();

        assert!(
            reloaded
                .registry_host
                .get("registry.carina-rs.dev")
                .unwrap()
                .pin()
                .is_none()
        );
        let error = match reloaded.prepare_registry_discovery_repin("registry.carina-rs.dev") {
            Ok(_) => panic!("an already cleared host must not preview invented replacement values"),
            Err(error) => error,
        };
        assert_eq!(
            error,
            RegistryLockRecoveryError::DiscoveryAlreadyUnpinned {
                host: "registry.carina-rs.dev".into(),
            }
        );
        assert!(error.to_string().contains("awaiting a new discovery pin"));
    }

    #[test]
    fn repin_identity_retains_signature_requirement_and_refuses_unsigned_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let mut lock = lock_with_registry_security_state(signature_pin("identity-a", "issuer-a"));
        lock.prepare_registry_identity_repin("carina-rs/aws")
            .unwrap()
            .commit()
            .unwrap();
        let source = match parse_provider_source("carina-rs/aws").unwrap() {
            ProviderSource::Registry(source) => source,
            ProviderSource::GithubDirect { .. } => unreachable!(),
        };
        let registry = resolved_registry(
            "registry.carina-rs.dev",
            "https://registry.carina-rs.dev/v1/providers/",
        );
        let lock_path = dir.path().join("carina-providers.lock");
        let mut persistent = PersistentLockFile::new(&mut lock, lock_path);

        let error = match verify_registry_lock_pin(
            &mut persistent,
            &source,
            "0.5.0",
            "pinned-shasum",
            &registry,
            None,
            true,
        ) {
            Ok(_) => panic!("repinning an identity must not permit an unsigned artifact"),
            Err(error) => error,
        };

        assert!(error.contains("signed to unsigned"), "{error}");
        assert!(error.contains("carina providers repin-identity"), "{error}");
    }

    #[test]
    fn rebootstrap_clears_freshness_together_and_preserves_other_security_state() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("carina-providers.lock");
        let mut lock = lock_with_registry_security_state(signature_pin("identity-a", "issuer-a"));
        let before_entry = lock.find_by_source("carina-rs/aws").unwrap().clone();

        let recovery = lock.prepare_registry_rebootstrap("carina-rs/aws").unwrap();
        let preview = recovery.freshness();
        assert_eq!(preview.sequence, Some(7));
        assert_eq!(preview.sequence_anchor, Some(5));
        recovery.commit().unwrap();

        let ratchets = lock.known_registry_ratchets("carina-rs/aws").unwrap();
        assert_eq!(ratchets.sequence, RegistrySequence::Absent);
        assert_known_yank_is_still_refused(&lock);
        assert_eq!(ratchets.signature, signature_pin("identity-a", "issuer-a"));
        assert!(ratchets.valid_until_present);
        assert!(ratchets.transparency_log_present);
        assert_eq!(
            lock.registry_sequence_anchor("carina-rs/aws"),
            RegistrySequenceAnchor::Unestablished
        );
        let after_entry = lock.find_by_source("carina-rs/aws").unwrap();
        assert_eq!(after_entry.sha256, before_entry.sha256);
        assert_eq!(after_entry.kind, before_entry.kind);

        lock.save(&lock_path).unwrap();
        let reloaded = LockFile::load(&lock_path).unwrap().unwrap();
        assert_eq!(
            reloaded.known_registry_ratchets("carina-rs/aws").unwrap(),
            ratchets
        );
        assert_eq!(
            reloaded.registry_sequence_anchor("carina-rs/aws"),
            RegistrySequenceAnchor::Unestablished
        );
    }

    #[test]
    fn rebootstrap_makes_missing_sequence_first_contact_instead_of_downgrade() {
        let source = match parse_provider_source("carina-rs/aws").unwrap() {
            ProviderSource::Registry(source) => source,
            ProviderSource::GithubDirect { .. } => unreachable!(),
        };
        let listing_without_sequence = RegistryVersions {
            sequence: None,
            valid_until: Some("2999-01-01T00:00:00Z".into()),
            versions: vec![RegistryVersion {
                version: "0.4.0".into(),
                yanked: true,
            }],
        };
        let mut lock = lock_with_registry_security_state(signature_pin("identity-a", "issuer-a"));

        let before = lock.known_registry_ratchets("carina-rs/aws").unwrap();
        let error = match ValidatedRegistryListing::validate(
            &source,
            &listing_without_sequence,
            &before,
            lock.registry_sequence_anchor("carina-rs/aws"),
        ) {
            Ok(_) => panic!("an established sequence anchor must reject a missing sequence"),
            Err(error) => error,
        };
        assert!(
            error.contains("registry sequence field disappeared"),
            "{error}"
        );

        lock.prepare_registry_rebootstrap("carina-rs/aws")
            .unwrap()
            .commit()
            .unwrap();

        let after = lock.known_registry_ratchets("carina-rs/aws").unwrap();
        assert_eq!(after.sequence, RegistrySequence::Absent);
        assert_eq!(
            lock.registry_sequence_anchor("carina-rs/aws"),
            RegistrySequenceAnchor::Unestablished
        );
        let validated = ValidatedRegistryListing::validate(
            &source,
            &listing_without_sequence,
            &after,
            lock.registry_sequence_anchor("carina-rs/aws"),
        )
        .expect("re-bootstrap must make a sequence-less listing first contact");
        let (_, validated_sequence) = validated.into_parts();
        assert_eq!(
            validated_sequence.into_anchor(),
            RegistrySequenceAnchor::Unestablished
        );
    }

    #[test]
    fn registry_recoveries_are_independent_and_order_independent() {
        let original = lock_with_registry_security_state(signature_pin("identity-a", "issuer-a"));
        let original_registry = original.provider[0].registry.as_ref().unwrap().clone();

        let mut repin_only = original.clone();
        repin_only
            .prepare_registry_identity_repin("carina-rs/aws")
            .unwrap()
            .commit()
            .unwrap();
        let repinned_registry = repin_only.provider[0].registry.as_ref().unwrap();
        assert_eq!(repinned_registry.sequence, original_registry.sequence);
        assert_eq!(
            repinned_registry.sequence_anchor,
            original_registry.sequence_anchor
        );

        let mut rebootstrap_only = original.clone();
        rebootstrap_only
            .prepare_registry_rebootstrap("carina-rs/aws")
            .unwrap()
            .commit()
            .unwrap();
        let rebootstrap_registry = rebootstrap_only.provider[0].registry.as_ref().unwrap();
        assert_eq!(rebootstrap_registry.signature, original_registry.signature);
        assert!(rebootstrap_registry.signature.is_required());
        assert_eq!(
            rebootstrap_registry.signature.identity_pin(),
            original_registry.signature.identity_pin()
        );

        let mut repin_then_rebootstrap = original.clone();
        repin_then_rebootstrap
            .prepare_registry_identity_repin("carina-rs/aws")
            .unwrap()
            .commit()
            .unwrap();
        repin_then_rebootstrap
            .prepare_registry_rebootstrap("carina-rs/aws")
            .unwrap()
            .commit()
            .unwrap();

        let mut rebootstrap_then_repin = original.clone();
        rebootstrap_then_repin
            .prepare_registry_rebootstrap("carina-rs/aws")
            .unwrap()
            .commit()
            .unwrap();
        rebootstrap_then_repin
            .prepare_registry_identity_repin("carina-rs/aws")
            .unwrap()
            .commit()
            .unwrap();

        assert_eq!(
            repin_then_rebootstrap.to_toml_string().unwrap(),
            rebootstrap_then_repin.to_toml_string().unwrap(),
            "the two recovery operations must commute"
        );

        let mut expected = original;
        let expected_registry = expected.provider[0].registry.as_mut().unwrap();
        expected_registry.sequence = RegistrySequence::Absent;
        expected_registry.sequence_anchor = RegistrySequenceAnchor::Unestablished;
        expected_registry.signature = RegistrySignatureProtection::RequiredUnpinned;
        assert_eq!(
            repin_then_rebootstrap.to_toml_string().unwrap(),
            expected.to_toml_string().unwrap(),
            "running both recoveries must change exactly freshness and the identity pin"
        );

        let final_entry = &repin_then_rebootstrap.provider[0];
        let final_registry = final_entry.registry.as_ref().unwrap();
        assert_eq!(final_entry.sha256, "pinned-shasum");
        assert!(final_registry.yanked_versions.contains("0.4.0"));
        assert!(final_registry.valid_until_present);
        assert!(final_registry.transparency_log_present);
    }

    #[test]
    fn prepared_registry_rebootstrap_clears_pair_and_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("carina-providers.lock");
        let mut lock = lock_with_registry_security_state(signature_pin("identity-a", "issuer-a"));

        let recovery = lock.prepare_registry_rebootstrap("carina-rs/aws").unwrap();
        let discarded = recovery.freshness();

        assert_eq!(discarded.sequence, Some(7));
        assert_eq!(discarded.sequence_anchor, Some(5));
        recovery.commit().unwrap();
        assert_eq!(
            lock.known_registry_ratchets("carina-rs/aws")
                .unwrap()
                .sequence,
            RegistrySequence::Absent
        );
        assert_eq!(
            lock.registry_sequence_anchor("carina-rs/aws"),
            RegistrySequenceAnchor::Unestablished
        );

        lock.save(&lock_path).unwrap();
        let reloaded = LockFile::load(&lock_path).unwrap().unwrap();
        assert_eq!(
            reloaded
                .known_registry_ratchets("carina-rs/aws")
                .unwrap()
                .sequence,
            RegistrySequence::Absent
        );
        assert_eq!(
            reloaded.registry_sequence_anchor("carina-rs/aws"),
            RegistrySequenceAnchor::Unestablished
        );
    }

    #[test]
    fn prepared_unpinned_rebootstrap_uses_the_lock_anchor_value() {
        let source = "carina-rs/aws";
        let mut lock = LockFile::default();
        lock.unpinned_registry_ratchets
            .merge(
                source.into(),
                RegistryRatchets {
                    sequence: RegistrySequence::Present(7),
                    ..RegistryRatchets::default()
                },
            )
            .unwrap();

        let recovery = lock.prepare_registry_rebootstrap(source).unwrap();
        assert_eq!(
            recovery.freshness(),
            RegistryFreshness {
                sequence: Some(7),
                sequence_anchor: None,
            }
        );
        recovery.commit().unwrap();

        assert_eq!(
            lock.known_registry_ratchets(source).unwrap().sequence,
            RegistrySequence::Absent
        );
        assert_eq!(
            lock.registry_sequence_anchor(source),
            RegistrySequenceAnchor::Unestablished
        );
    }

    #[test]
    fn dropping_prepared_recoveries_leaves_the_lock_unchanged() {
        let mut lock = lock_with_registry_security_state(signature_pin("identity-a", "issuer-a"));
        let before = lock.known_registry_ratchets("carina-rs/aws").unwrap();
        let before_anchor = lock.registry_sequence_anchor("carina-rs/aws");

        drop(
            lock.prepare_registry_identity_repin("carina-rs/aws")
                .unwrap(),
        );
        assert_eq!(
            lock.known_registry_ratchets("carina-rs/aws").unwrap(),
            before
        );
        assert_eq!(
            lock.registry_sequence_anchor("carina-rs/aws"),
            before_anchor
        );

        drop(lock.prepare_registry_rebootstrap("carina-rs/aws").unwrap());
        assert_eq!(
            lock.known_registry_ratchets("carina-rs/aws").unwrap(),
            before
        );
        assert_eq!(
            lock.registry_sequence_anchor("carina-rs/aws"),
            before_anchor
        );
    }

    #[test]
    fn recovery_remediations_name_operations_without_lock_entry_deletion_advice() {
        assert!(IDENTITY_REPIN_REMEDIATION.contains("carina providers repin-identity <provider>"));
        assert!(
            SEQUENCE_REBOOTSTRAP_REMEDIATION.contains("carina providers re-bootstrap <provider>")
        );
        assert!(DISCOVERY_REPIN_REMEDIATION.contains("carina providers repin-discovery <host>"));
        for remediation in [
            IDENTITY_REPIN_REMEDIATION,
            SEQUENCE_REBOOTSTRAP_REMEDIATION,
            DISCOVERY_REPIN_REMEDIATION,
        ] {
            let remediation = remediation.to_ascii_lowercase();
            assert!(!remediation.contains("delete"), "{remediation}");
            assert!(!remediation.contains("remove"), "{remediation}");
            assert!(!remediation.contains("lock entry"), "{remediation}");
        }
    }

    #[test]
    fn registry_ratchet_storage_load_attaches_unpinned_to_provider() {
        let serialized = r#"version = 3

[registry_host."registry.carina-rs.dev"]
discovery_pin_present = true

[registry_host."registry.carina-rs.dev".discovery_values]
"providers.v1" = "https://registry.carina-rs.dev/v1/providers/"

[[provider]]
name = "aws"
source = "carina-rs/aws"
mode = "version"
version = "0.5.0"
sha256 = "abc"

[provider.registry]
resolved_hostname = "registry.carina-rs.dev"
sequence_present = false
sequence_anchor_established = false
valid_until_present = false
signature_present = false
transparency_log_present = false

[unpinned_registry_ratchets."carina-rs/aws"]
sequence_present = true
sequence = 7
valid_until_present = true
yanked_versions = ["0.4.0"]
signature_present = true
certificate_identity = "identity-a"
certificate_oidc_issuer = "issuer-a"
transparency_log_present = true
"#;

        let lock = LockFile::from_toml_str(serialized, Path::new("carina-providers.lock")).unwrap();
        let registry = lock.provider[0].registry.as_ref().unwrap();
        assert_eq!(registry.sequence.value(), Some(7));
        assert!(registry.valid_until_present);
        assert!(registry.yanked_versions.contains("0.4.0"));
        assert_eq!(registry.signature, signature_pin("identity-a", "issuer-a"));
        assert!(registry.transparency_log_present);
        assert!(lock.unpinned_registry_ratchets.is_empty());
        assert!(
            !lock
                .to_toml_string()
                .unwrap()
                .contains("[unpinned_registry_ratchets."),
            "load-time attachment must leave each source in one persisted location"
        );
    }

    #[test]
    fn registry_ratchet_storage_store_clears_shadow_for_pinned_provider() {
        let source = "carina-rs/aws";
        let mut lock = LockFile::default();
        lock.upsert_registry(
            LockEntry {
                name: "aws".into(),
                source: source.into(),
                kind: LockEntryKind::Version {
                    version: "0.5.0".into(),
                    constraint: None,
                },
                sha256: "abc".into(),
                registry: Some(RegistryLock {
                    resolved_hostname: "registry.carina-rs.dev".into(),
                    sequence: RegistrySequence::Absent,
                    sequence_anchor: RegistrySequenceAnchor::Unestablished,
                    valid_until_present: false,
                    yanked_versions: YankedRegistryVersions::default(),
                    signature: RegistrySignatureProtection::NotRequired,
                    transparency_log_present: false,
                }),
            },
            registry_host_lock("https://registry.carina-rs.dev/v1/providers/"),
        )
        .unwrap();
        let observed = RegistryRatchets {
            sequence: RegistrySequence::Present(7),
            valid_until_present: true,
            ..RegistryRatchets::default()
        };
        lock.unpinned_registry_ratchets
            .merge(source.into(), observed.clone())
            .unwrap();

        lock.store_registry_ratchets(source, observed).unwrap();

        assert!(lock.unpinned_registry_ratchets.get(source).is_none());
        let registry = lock.provider[0].registry.as_ref().unwrap();
        assert_eq!(registry.sequence.value(), Some(7));
        assert!(registry.valid_until_present);
    }

    #[test]
    fn registry_provider_writes_always_create_referenced_host_records() {
        let mut lock = LockFile::default();

        for (name, source, hostname) in [
            ("aws", "carina-rs/aws", "registry.carina-rs.dev"),
            (
                "random",
                "registry.example.test/acme/random",
                "registry.example.test",
            ),
        ] {
            let registry_source = match parse_provider_source(source).unwrap() {
                ProviderSource::Registry(source) => source,
                ProviderSource::GithubDirect { .. } => unreachable!(),
            };
            let validated = ValidatedRegistryListing::validate(
                &registry_source,
                &RegistryVersions {
                    sequence: None,
                    valid_until: None,
                    versions: Vec::new(),
                },
                &RegistryRatchets::default(),
                RegistrySequenceAnchor::Unestablished,
            )
            .unwrap();
            let (_, validated_sequence) = validated.into_parts();
            let mut persistent =
                PersistentLockFile::new(&mut lock, PathBuf::from("carina-providers.lock"));

            persistent
                .upsert_registry_provider(RegistryProviderLockEntry {
                    name: name.into(),
                    source: source.into(),
                    kind: LockEntryKind::Version {
                        version: "1.0.0".into(),
                        constraint: None,
                    },
                    sha256: format!("{name}-sha256"),
                    registry: resolved_registry(
                        hostname,
                        &format!("https://{hostname}/v1/providers/"),
                    ),
                    validated_sequence,
                })
                .unwrap();

            for entry in &lock.provider {
                let Some(registry) = &entry.registry else {
                    continue;
                };
                assert!(
                    lock.registry_host
                        .contains_key(registry.resolved_hostname()),
                    "registry provider {:?} references missing host {:?}",
                    entry.source,
                    registry.resolved_hostname()
                );
            }
        }

        assert_eq!(lock.provider.len(), 2);
        assert_eq!(lock.registry_host.len(), 2);
    }

    #[test]
    fn upsert_replaces_existing_entry_by_source() {
        let source = "github.com/carina-rs/carina-provider-awscc";
        let mut lock = LockFile::default();
        lock.upsert(version_entry(source, "0.1.0"));
        lock.upsert(version_entry(source, "0.2.0"));

        assert_eq!(lock.provider.len(), 1);
        match &lock.provider[0].kind {
            LockEntryKind::Version { version, .. } => assert_eq!(version, "0.2.0"),
            other => panic!("expected Version variant, got {other:?}"),
        }
    }

    #[test]
    fn lock_upsert_rejects_conflicting_registry_identity_pin() {
        let mut lock = LockFile::default();
        lock.upsert_registry(
            LockEntry {
                name: "aws".into(),
                source: "carina-rs/aws".into(),
                kind: LockEntryKind::Version {
                    version: "0.5.0".into(),
                    constraint: None,
                },
                sha256: "recorded".into(),
                registry: Some(RegistryLock {
                    resolved_hostname: "registry.carina-rs.dev".into(),
                    sequence: RegistrySequence::Present(100),
                    sequence_anchor: RegistrySequenceAnchor::Established(100),
                    valid_until_present: true,
                    yanked_versions: YankedRegistryVersions::default(),
                    signature: signature_pin("id-a", "issuer-a"),
                    transparency_log_present: true,
                }),
            },
            registry_host_lock("https://registry.carina-rs.dev/v1/providers/"),
        )
        .unwrap();

        let hostile = lock
            .upsert_registry(
                LockEntry {
                    name: "aws".into(),
                    source: "carina-rs/aws".into(),
                    kind: LockEntryKind::Version {
                        version: "0.6.0".into(),
                        constraint: None,
                    },
                    sha256: "proposed".into(),
                    registry: Some(RegistryLock {
                        resolved_hostname: "registry.carina-rs.dev".into(),
                        sequence: RegistrySequence::Absent,
                        sequence_anchor: RegistrySequenceAnchor::Unestablished,
                        valid_until_present: false,
                        yanked_versions: YankedRegistryVersions::default(),
                        signature: signature_pin("EVIL", "EVIL-issuer"),
                        transparency_log_present: false,
                    }),
                },
                registry_host_lock("https://registry.carina-rs.dev/v1/providers/"),
            )
            .expect_err("a conflicting identity pin must be rejected");
        assert_eq!(
            hostile.left,
            IdentityPin {
                certificate_identity: "EVIL".into(),
                certificate_oidc_issuer: "EVIL-issuer".into(),
            }
        );
        assert_eq!(
            hostile.right,
            IdentityPin {
                certificate_identity: "id-a".into(),
                certificate_oidc_issuer: "issuer-a".into(),
            }
        );

        let entry = lock.find_by_source("carina-rs/aws").unwrap();
        assert_eq!(
            entry.kind,
            LockEntryKind::Version {
                version: "0.5.0".into(),
                constraint: None,
            },
            "a conflicting entry must not replace any part of the recorded lock"
        );
        let registry = entry.registry.as_ref().unwrap();
        assert_eq!(registry.sequence, RegistrySequence::Present(100));
        assert!(registry.valid_until_present);
        assert!(registry.transparency_log_present);
        assert_eq!(registry.signature, signature_pin("id-a", "issuer-a"));
    }

    #[test]
    fn sha256_file_matches_known_digest() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("test.bin");
        fs::File::create(&file_path)
            .unwrap()
            .write_all(b"hello world")
            .unwrap();
        assert_eq!(
            sha256_file(&file_path).unwrap(),
            "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
        );
    }

    /// `find` and `find_by_source_and_sha` now pattern-match on the kind, so a
    /// revision-mode entry never matches a version-mode query and vice versa.
    /// This is the type-level replacement for the runtime guard in #2028.
    #[test]
    fn find_queries_respect_entry_kind() {
        let source = "github.com/carina-rs/carina-provider-awscc";
        let sha = "deadbeefcafe";
        let mut lock = LockFile::default();
        lock.upsert(revision_entry(source, "main", sha));

        // Version-mode query does not match a revision entry.
        assert!(lock.find(source, "0.5.2").is_none());
        // Revision-by-sha query matches.
        assert!(lock.find_by_source_and_sha(source, sha).is_some());

        // Reverse: version-mode entry doesn't answer a revision query.
        let mut lock = LockFile::default();
        lock.upsert(version_entry(source, "0.5.2"));
        assert!(lock.find(source, "0.5.2").is_some());
        assert!(lock.find_by_source_and_sha(source, sha).is_none());
    }

    /// #2028 regression, now enforced by the type: `try_reuse_locked_version`
    /// pattern-matches on `LockEntryKind::Version`, so revision-mode entries
    /// cannot leak their (non-existent) version string into a URL.
    #[test]
    fn try_reuse_skips_revision_mode_entry() {
        let source = "github.com/carina-rs/carina-provider-awscc";
        let mut lock = LockFile::default();
        lock.upsert(revision_entry(source, "main", "deadbeefcafe"));
        let config = provider_config(source, None);

        assert!(
            try_reuse_locked_version(source, &config, &lock).is_none(),
            "revision-mode lock entries must not be reused for version-mode configs"
        );
    }

    #[test]
    fn try_reuse_returns_locked_version_for_version_mode_entry() {
        let source = "github.com/carina-rs/carina-provider-awscc";
        let mut lock = LockFile::default();
        lock.upsert(version_entry(source, "0.5.2"));
        let config = provider_config(source, None);

        assert_eq!(
            try_reuse_locked_version(source, &config, &lock),
            Some("0.5.2".to_string())
        );
    }

    #[test]
    fn try_reuse_registry_revision_requires_matching_revision_metadata() {
        let source = "carina-rs/aws";
        let config = provider_config(source, Some("main"));

        let mut matching_lock = LockFile::default();
        matching_lock.upsert(LockEntry {
            name: "aws".into(),
            source: source.into(),
            kind: LockEntryKind::RegistryRevision {
                revision: "main".into(),
                version: "0.0.0-main.1.aaa".into(),
            },
            sha256: "abc".into(),
            registry: None,
        });
        assert_eq!(
            try_reuse_locked_version(source, &config, &matching_lock),
            Some("0.0.0-main.1.aaa".into())
        );

        let mut plain_version_lock = LockFile::default();
        plain_version_lock.upsert(version_entry(source, "0.5.0"));
        assert!(
            try_reuse_locked_version(source, &config, &plain_version_lock).is_none(),
            "registry revision must not reuse a plain version lock"
        );
    }

    #[test]
    fn try_reuse_rejects_registry_revision_lock_when_config_has_no_revision_or_version() {
        let source = "carina-rs/aws";
        let config = provider_config(source, None);
        let mut lock = LockFile::default();
        lock.upsert(LockEntry {
            name: "aws".into(),
            source: source.into(),
            kind: LockEntryKind::RegistryRevision {
                revision: "main".into(),
                version: "0.0.0-main.10.bbb".into(),
            },
            sha256: "abc".into(),
            registry: None,
        });

        assert!(
            try_reuse_locked_version(source, &config, &lock).is_none(),
            "unconstrained version-mode configs must not reuse registry-revision locks"
        );
    }

    #[test]
    fn try_reuse_accepts_plain_version_lock_when_config_has_no_revision_or_version() {
        let source = "carina-rs/aws";
        let config = provider_config(source, None);
        let mut lock = LockFile::default();
        lock.upsert(version_entry(source, "0.5.0"));

        assert_eq!(
            try_reuse_locked_version(source, &config, &lock),
            Some("0.5.0".into())
        );
    }

    #[test]
    fn resolve_all_copies_file_provider() {
        let tmp = tempfile::tempdir().unwrap();
        let wasm_path = tmp.path().join("my-provider.wasm");
        fs::write(&wasm_path, b"fake wasm content").unwrap();

        let source = format!("file://{}", wasm_path.display());
        let providers = vec![ProviderConfig {
            name: "test".into(),
            source: Some(source.clone()),
            version: None,
            revision: None,
            unresolved_attributes: IndexMap::new(),
            binding: None,
            is_default: true,
            attributes: IndexMap::new(),
            default_tags: IndexMap::new(),
        }];

        let result = resolve_all(tmp.path(), &providers, LockMode::Normal).unwrap();
        let dest = result.get("test").expect("provider should be resolved");
        assert!(dest.exists());
        assert!(dest.starts_with(tmp.path().join(".carina/providers/file")));

        let lock = LockFile::load(&tmp.path().join("carina-providers.lock"))
            .unwrap()
            .unwrap();
        let entry = lock.find_by_source(&source).unwrap();
        assert_eq!(entry.kind, LockEntryKind::File);
        assert!(!entry.sha256.is_empty());
    }

    #[test]
    fn resolve_all_errors_on_missing_file_provider() {
        let tmp = tempfile::tempdir().unwrap();
        let providers = vec![ProviderConfig {
            name: "test".into(),
            source: Some("file:///nonexistent/path.wasm".into()),
            version: None,
            revision: None,
            unresolved_attributes: IndexMap::new(),
            binding: None,
            is_default: true,
            attributes: IndexMap::new(),
            default_tags: IndexMap::new(),
        }];
        let err = resolve_all(tmp.path(), &providers, LockMode::Normal).unwrap_err();
        assert!(err.contains("not found"));
    }

    #[derive(Clone)]
    enum FakeHttpAnswer {
        Response { status: u16, body: Vec<u8> },
        Redirect { status: u16, location: String },
    }

    #[derive(Default)]
    struct FakeRegistryHttp {
        responses: HashMap<String, FakeHttpAnswer>,
        downloads: HashMap<String, Vec<u8>>,
        requested: std::sync::Mutex<Vec<String>>,
    }

    impl FakeRegistryHttp {
        fn response(mut self, url: &str, status: u16, body: &[u8]) -> Self {
            self.responses.insert(
                url.to_string(),
                FakeHttpAnswer::Response {
                    status,
                    body: body.to_vec(),
                },
            );
            self
        }

        fn json(mut self, url: &str, body: &str) -> Self {
            self.responses.insert(
                url.to_string(),
                FakeHttpAnswer::Response {
                    status: 200,
                    body: body.as_bytes().to_vec(),
                },
            );
            self
        }

        fn bytes(mut self, url: &str, body: &[u8]) -> Self {
            self.responses.insert(
                url.to_string(),
                FakeHttpAnswer::Response {
                    status: 200,
                    body: body.to_vec(),
                },
            );
            self
        }

        fn redirect(mut self, url: &str, status: u16, location: &str) -> Self {
            assert!((300..400).contains(&status));
            self.responses.insert(
                url.to_string(),
                FakeHttpAnswer::Redirect {
                    status,
                    location: location.to_string(),
                },
            );
            self
        }

        fn downloadable_bytes(mut self, url: &str, body: &[u8]) -> Self {
            self.downloads.insert(url.to_string(), body.to_vec());
            self
        }

        fn was_requested(&self, needle: &str) -> bool {
            self.requested
                .lock()
                .unwrap()
                .iter()
                .any(|url| url.contains(needle))
        }

        fn request_count(&self, url: &str) -> usize {
            self.requested
                .lock()
                .unwrap()
                .iter()
                .filter(|requested| requested.as_str() == url)
                .count()
        }
    }

    impl RegistryHttp for FakeRegistryHttp {
        fn get(&self, request: RegistryHttpRequest<'_>) -> Result<HttpResponse, String> {
            let url = request.url();
            let follows_redirects = matches!(request, RegistryHttpRequest::Resource(_));
            let mut current_url = url.to_string();
            for _ in 0..=10 {
                self.requested.lock().unwrap().push(current_url.clone());
                match self.responses.get(&current_url).cloned() {
                    Some(FakeHttpAnswer::Response { status, body }) => {
                        return Ok(if (200..300).contains(&status) {
                            HttpResponse::Success { body }
                        } else {
                            HttpResponse::Failure { status }
                        });
                    }
                    Some(FakeHttpAnswer::Redirect { location, .. }) if follows_redirects => {
                        current_url = location;
                    }
                    Some(FakeHttpAnswer::Redirect { status, .. }) => {
                        return Ok(HttpResponse::Failure { status });
                    }
                    None => return Err(format!("unexpected test URL: {current_url}")),
                }
            }
            Err(format!("too many test redirects from {url}"))
        }

        fn download_to_file(&self, url: &str, dest: &Path) -> Result<(), String> {
            self.requested.lock().unwrap().push(url.to_string());
            let body = self
                .downloads
                .get(url)
                .ok_or_else(|| format!("unexpected test download URL: {url}"))?;
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent)
                    .map_err(|e| format!("Failed to create directory {}: {e}", parent.display()))?;
            }
            fs::write(dest, body)
                .map_err(|e| format!("Failed to write file {}: {e}", dest.display()))?;
            Ok(())
        }
    }

    fn sha256_bytes(bytes: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        format!("{:x}", hasher.finalize())
    }

    fn registry_http(download_body: &[u8], shasum: &str) -> FakeRegistryHttp {
        FakeRegistryHttp::default()
            .json(
                "https://registry.carina-rs.dev/.well-known/carina.json",
                r#"{"providers.v1":"/v1/providers/"}"#,
            )
            .json(
                "https://registry.carina-rs.dev/v1/providers/carina-rs/aws/versions",
                r#"{"sequence":7,"valid_until":"2999-01-01T00:00:00Z","versions":[{"version":"0.5.0","protocols":["1"]}]}"#,
            )
            .json(
                "https://registry.carina-rs.dev/v1/providers/carina-rs/aws/0.5.0/download",
                &format!(
                    r#"{{
                        "protocols":["1"],
                        "filename":"carina-provider-aws-v0.5.0.wasm",
                        "download_url":"https://downloads.example.test/aws.wasm",
                        "shasum":"{shasum}",
                        "shasum_authored_by":"registry"
                    }}"#
                ),
            )
            .bytes("https://downloads.example.test/aws.wasm", download_body)
            .downloadable_bytes("https://downloads.example.test/aws.wasm", download_body)
    }

    fn registry_http_with_discovery(
        discovery_document: &str,
        api_base_url: &str,
    ) -> FakeRegistryHttp {
        const ARTIFACT: &[u8] = b"discovery pin regression fixture";

        let shasum = sha256_bytes(ARTIFACT);
        let versions_url = join_registry_url(api_base_url, "/carina-rs/aws/versions");
        let download_url = join_registry_url(api_base_url, "/carina-rs/aws/0.5.0/download");
        FakeRegistryHttp::default()
            .json(
                "https://registry.carina-rs.dev/.well-known/carina.json",
                discovery_document,
            )
            .json(
                &versions_url,
                r#"{"sequence":7,"valid_until":"2999-01-01T00:00:00Z","versions":[{"version":"0.5.0","protocols":["1"]}]}"#,
            )
            .json(
                &download_url,
                &format!(
                    r#"{{"protocols":["1"],"filename":"aws.wasm","download_url":"https://downloads.example.test/discovery-pin.wasm","shasum":"{shasum}","shasum_authored_by":"registry"}}"#
                ),
            )
            .bytes("https://downloads.example.test/discovery-pin.wasm", ARTIFACT)
            .downloadable_bytes(
                "https://downloads.example.test/discovery-pin.wasm",
                ARTIFACT,
            )
    }

    fn signed_registry_http(bundle: &[u8], bundle_status: u16) -> FakeRegistryHttp {
        let shasum = sha256_bytes(SIGNED_FIXTURE_ARTIFACT);
        FakeRegistryHttp::default()
            .json(
                "https://registry.carina-rs.dev/.well-known/carina.json",
                r#"{"providers.v1":"/v1/providers/"}"#,
            )
            .json(
                "https://registry.carina-rs.dev/v1/providers/carina-rs/aws/versions",
                r#"{"sequence":7,"valid_until":"2999-01-01T00:00:00Z","versions":[{"version":"0.5.0","protocols":["1"]}]}"#,
            )
            .json(
                "https://registry.carina-rs.dev/v1/providers/carina-rs/aws/0.5.0/download",
                &format!(
                    r#"{{
                        "protocols":["1"],
                        "filename":"carina-provider-aws-v0.5.0.wasm",
                        "download_url":"https://downloads.example.test/aws.wasm",
                        "shasum":"{shasum}",
                        "shasum_authored_by":"registry",
                        "signature":{{
                            "type":"sigstore-bundle",
                            "certificate_identity":"{SIGNED_FIXTURE_IDENTITY}",
                            "certificate_oidc_issuer":"{SIGNED_FIXTURE_ISSUER}",
                            "bundle_url":"{SIGNED_FIXTURE_BUNDLE_URL}"
                        }}
                    }}"#
                ),
            )
            .response(SIGNED_FIXTURE_BUNDLE_URL, bundle_status, bundle)
            .downloadable_bytes(
                "https://downloads.example.test/aws.wasm",
                SIGNED_FIXTURE_ARTIFACT,
            )
    }

    fn registry_http_with_listing(
        download_body: &[u8],
        shasum: &str,
        listing: &str,
    ) -> FakeRegistryHttp {
        registry_http(download_body, shasum).json(REGISTRY_VERSIONS_URL, listing)
    }

    fn saved_lock_contents(base_dir: &Path) -> String {
        fs::read_to_string(base_dir.join("carina-providers.lock"))
            .expect("an accepted registry ratchet must already be durable")
    }

    fn saved_registry_ratchets(base_dir: &Path) -> RegistryRatchets {
        let lock_path = base_dir.join("carina-providers.lock");
        LockFile::load(&lock_path)
            .unwrap()
            .unwrap_or_default()
            .known_registry_ratchets("carina-rs/aws")
            .unwrap()
    }

    fn saved_registry_sequence_anchor(base_dir: &Path) -> RegistrySequenceAnchor {
        let lock_path = base_dir.join("carina-providers.lock");
        LockFile::load(&lock_path)
            .unwrap()
            .unwrap_or_default()
            .registry_sequence_anchor("carina-rs/aws")
    }

    #[test]
    fn registry_ratchet_storage_successful_resolve_has_single_location() {
        let dir = tempfile::tempdir().unwrap();
        let config = provider_config("carina-rs/aws", None);

        resolve_single_config_with_http(
            dir.path(),
            &config,
            &signed_registry_http(SIGNED_FIXTURE_BUNDLE, 200),
        )
        .unwrap();

        let lock_contents = saved_lock_contents(dir.path());
        assert!(
            lock_contents.contains("[provider.registry]"),
            "{lock_contents}"
        );
        assert_eq!(
            lock_contents.matches("sequence = 7").count(),
            1,
            "registry ratchets must be stored exactly once: {lock_contents}"
        );
        assert!(
            !lock_contents.contains("[unpinned_registry_ratchets.\"carina-rs/aws\"]"),
            "a pinned provider must not retain a shadow ratchet entry: {lock_contents}"
        );
    }

    #[test]
    fn deleting_registry_entry_drops_the_security_baseline() {
        let dir = tempfile::tempdir().unwrap();
        let config = provider_config("carina-rs/aws", None);
        resolve_single_config_with_http(
            dir.path(),
            &config,
            &signed_registry_http(SIGNED_FIXTURE_BUNDLE, 200),
        )
        .unwrap();

        let lock_path = dir.path().join("carina-providers.lock");
        let pinned = fs::read_to_string(&lock_path).unwrap();
        let provider_start = pinned
            .find("[[provider]]")
            .expect("the successful resolve must pin a provider entry");
        let retained_top_level = pinned
            .find("[unpinned_registry_ratchets.")
            .map(|start| &pinned[start..])
            .unwrap_or("");
        let edited = format!("{}{}", &pinned[..provider_start], retained_top_level);
        assert!(!edited.contains("[[provider]]"), "{edited}");
        fs::write(&lock_path, edited).unwrap();

        let shasum = sha256_bytes(SIGNED_FIXTURE_ARTIFACT);
        let unsigned_http = registry_http_with_listing(
            SIGNED_FIXTURE_ARTIFACT,
            &shasum,
            r#"{"sequence":10000000,"valid_until":"2999-01-01T00:00:00Z","versions":[{"version":"0.5.0","protocols":["1"]}]}"#,
        );
        let path = resolve_single_config_with_http(dir.path(), &config, &unsigned_http)
            .expect("manual entry deletion demonstrates why recovery must preserve ratchets");

        assert_eq!(fs::read(path).unwrap(), SIGNED_FIXTURE_ARTIFACT);
        assert_eq!(
            saved_registry_sequence_anchor(dir.path()),
            RegistrySequenceAnchor::Established(10_000_000)
        );
    }

    #[test]
    fn registry_security_honest_high_sequence_first_contact_is_accepted() {
        const ARTIFACT: &[u8] = b"honest high-sequence artifact";
        let shasum = sha256_bytes(ARTIFACT);

        for sequence in [1_000_000_u64, 10_000_000] {
            let dir = tempfile::tempdir().unwrap();
            let listing = format!(
                r#"{{"sequence":{sequence},"versions":[{{"version":"0.5.0","protocols":["1"]}}]}}"#
            );
            let http = registry_http_with_listing(ARTIFACT, &shasum, &listing);

            let path = resolve_single_config_with_http(
                dir.path(),
                &provider_config("carina-rs/aws", None),
                &http,
            )
            .unwrap_or_else(|error| {
                panic!("honest first contact at sequence {sequence} was refused: {error}")
            });

            assert_eq!(fs::read(path).unwrap(), ARTIFACT);
            assert_eq!(
                saved_registry_sequence_anchor(dir.path()),
                RegistrySequenceAnchor::Established(sequence)
            );
        }
    }

    #[test]
    fn registry_security_established_zero_sequence_is_not_first_contact() {
        const ARTIFACT: &[u8] = b"established zero-sequence artifact";
        let dir = tempfile::tempdir().unwrap();
        let config = provider_config("carina-rs/aws", None);
        let shasum = sha256_bytes(ARTIFACT);
        let baseline_http = registry_http_with_listing(
            ARTIFACT,
            &shasum,
            r#"{"sequence":0,"versions":[{"version":"0.5.0","protocols":["1"]}]}"#,
        );
        resolve_single_config_with_http(dir.path(), &config, &baseline_http).unwrap();
        assert_eq!(
            saved_registry_sequence_anchor(dir.path()),
            RegistrySequenceAnchor::Established(0)
        );

        let hostile_sequence = MAX_SEQUENCE_FAST_FORWARD + 1;
        let listing = format!(
            r#"{{"sequence":{hostile_sequence},"versions":[{{"version":"0.5.0","protocols":["1"]}}]}}"#
        );
        let hostile_http = registry_http_with_listing(ARTIFACT, &shasum, &listing);
        let error =
            resolve_single_config_with_http(dir.path(), &config, &hostile_http).unwrap_err();

        assert!(error.contains("sequence fast-forward"), "{error}");
        assert_eq!(
            saved_registry_ratchets(dir.path()).sequence.value(),
            Some(0)
        );
        assert_eq!(
            saved_registry_sequence_anchor(dir.path()),
            RegistrySequenceAnchor::Established(0)
        );
    }

    #[test]
    fn registry_security_failed_first_contact_sequence_is_not_an_anchor() {
        const ARTIFACT: &[u8] = b"honest first-contact artifact";
        let dir = tempfile::tempdir().unwrap();
        let config = provider_config("carina-rs/aws", None);
        let hostile_sequence = i64::MAX as u64;
        let hostile_listing = format!(
            r#"{{"sequence":{hostile_sequence},"versions":[{{"version":"0.5.0","protocols":["1"]}}]}}"#
        );
        let hostile_http = FakeRegistryHttp::default()
            .json(
                "https://registry.carina-rs.dev/.well-known/carina.json",
                r#"{"providers.v1":"/v1/providers/"}"#,
            )
            .json(REGISTRY_VERSIONS_URL, &hostile_listing);

        let error =
            resolve_single_config_with_http(dir.path(), &config, &hostile_http).unwrap_err();
        assert_eq!(
            saved_registry_ratchets(dir.path()).sequence.value(),
            Some(hostile_sequence),
            "the accepted listing observation must survive its downstream failure"
        );
        assert_eq!(
            saved_registry_sequence_anchor(dir.path()),
            RegistrySequenceAnchor::Unestablished,
            "a failed first contact must not establish a rollback anchor"
        );
        assert!(error.contains("unexpected test URL"), "{error}");
        assert!(hostile_http.was_requested("/0.5.0/download"));

        let shasum = sha256_bytes(ARTIFACT);
        let honest_http = registry_http_with_listing(
            ARTIFACT,
            &shasum,
            r#"{"sequence":42,"versions":[{"version":"0.5.0","protocols":["1"]}]}"#,
        );
        let path = resolve_single_config_with_http(dir.path(), &config, &honest_http)
            .expect("an unpinned observation must not become a rollback floor");
        assert_eq!(fs::read(path).unwrap(), ARTIFACT);
        assert_eq!(
            saved_registry_sequence_anchor(dir.path()),
            RegistrySequenceAnchor::Established(42)
        );
        assert_eq!(
            saved_registry_ratchets(dir.path()).sequence.value(),
            Some(hostile_sequence),
            "successful recovery must not erase the durable failed observation"
        );
    }

    #[test]
    fn registry_security_sequence_ceiling_cannot_walk_by_maximum_steps() {
        const ARTIFACT: &[u8] = b"sequence ceiling walk artifact";
        let dir = tempfile::tempdir().unwrap();
        let config = provider_config("carina-rs/aws", None);
        let shasum = sha256_bytes(ARTIFACT);
        let baseline_http = registry_http_with_listing(
            ARTIFACT,
            &shasum,
            r#"{"sequence":10,"versions":[{"version":"0.5.0","protocols":["1"]}]}"#,
        );
        resolve_single_config_with_http(dir.path(), &config, &baseline_http).unwrap();

        let accepted_sequence = 10 + MAX_SEQUENCE_FAST_FORWARD;
        let accepted_listing = format!(
            r#"{{"sequence":{accepted_sequence},"versions":[{{"version":"0.5.0","protocols":["1"]}}]}}"#
        );
        let hostile_shasum = sha256_bytes(b"hostile accepted observation");
        let accepted_http =
            registry_http_with_listing(ARTIFACT, &hostile_shasum, &accepted_listing);
        let error =
            resolve_single_config_with_http(dir.path(), &config, &accepted_http).unwrap_err();
        assert!(error.contains("shasum pin mismatch"), "{error}");

        for attempt in 1..=4 {
            let sequence = accepted_sequence + attempt;
            let listing = format!(
                r#"{{"sequence":{sequence},"versions":[{{"version":"0.5.0","protocols":["1"]}}]}}"#
            );
            let hostile_http = registry_http_with_listing(ARTIFACT, &hostile_shasum, &listing);

            let error =
                resolve_single_config_with_http(dir.path(), &config, &hostile_http).unwrap_err();
            assert!(
                error.contains("sequence fast-forward"),
                "attempt {attempt}: {error}"
            );
            assert!(!hostile_http.was_requested("/0.5.0/download"));
            assert_eq!(
                saved_registry_ratchets(dir.path()).sequence.value(),
                Some(accepted_sequence),
                "attempt {attempt} advanced the recorded observation"
            );
            assert_eq!(
                saved_registry_sequence_anchor(dir.path()),
                RegistrySequenceAnchor::Established(10),
                "attempt {attempt} moved the established anchor"
            );
        }
    }

    #[test]
    fn registry_security_sequence_ceiling_is_cumulative_between_successful_resolves() {
        const ARTIFACT: &[u8] = b"cumulative sequence ceiling artifact";
        let dir = tempfile::tempdir().unwrap();
        let config = provider_config("carina-rs/aws", None);
        let shasum = sha256_bytes(ARTIFACT);
        let baseline_http = registry_http_with_listing(
            ARTIFACT,
            &shasum,
            r#"{"sequence":10,"versions":[{"version":"0.5.0","protocols":["1"]}]}"#,
        );
        resolve_single_config_with_http(dir.path(), &config, &baseline_http).unwrap();

        let accepted_sequence = 10 + MAX_SEQUENCE_FAST_FORWARD;
        let first_listing = format!(
            r#"{{"sequence":{accepted_sequence},"versions":[{{"version":"0.5.0","protocols":["1"]}}]}}"#
        );
        let hostile_shasum = sha256_bytes(b"hostile cumulative shasum");
        let first_http = registry_http_with_listing(ARTIFACT, &hostile_shasum, &first_listing);
        let error = resolve_single_config_with_http(dir.path(), &config, &first_http).unwrap_err();
        assert!(error.contains("shasum pin mismatch"), "{error}");
        assert_eq!(
            saved_registry_ratchets(dir.path()).sequence.value(),
            Some(accepted_sequence),
            "an accepted listing observation must survive its downstream failure"
        );
        assert_eq!(
            saved_registry_sequence_anchor(dir.path()),
            RegistrySequenceAnchor::Established(10),
            "a downstream failure must not advance the established anchor"
        );

        let walked_sequence = accepted_sequence + 1;
        let second_listing = format!(
            r#"{{"sequence":{walked_sequence},"versions":[{{"version":"0.5.0","protocols":["1"]}}]}}"#
        );
        let second_http = registry_http_with_listing(ARTIFACT, &hostile_shasum, &second_listing);
        let error = resolve_single_config_with_http(dir.path(), &config, &second_http).unwrap_err();
        assert!(error.contains("sequence fast-forward"), "{error}");
        assert_eq!(
            saved_registry_ratchets(dir.path()).sequence.value(),
            Some(accepted_sequence),
            "failed resolves must not move the cumulative ceiling"
        );
        assert_eq!(
            saved_registry_sequence_anchor(dir.path()),
            RegistrySequenceAnchor::Established(10)
        );
    }

    #[test]
    fn registry_ratchet_sequence_survives_downstream_shasum_failure() {
        const EXPECTED_ARTIFACT: &[u8] = b"expected sequence-ratchet artifact";
        let dir = tempfile::tempdir().unwrap();
        let shasum = sha256_bytes(EXPECTED_ARTIFACT);
        let config = provider_config("carina-rs/aws", None);
        let baseline_http = registry_http_with_listing(
            EXPECTED_ARTIFACT,
            &shasum,
            r#"{"sequence":100,"valid_until":"2999-01-01T00:00:00Z","versions":[{"version":"0.5.0","protocols":["1"]}]}"#,
        );
        let cached_path =
            resolve_single_config_with_http(dir.path(), &config, &baseline_http).unwrap();
        fs::remove_file(cached_path).unwrap();

        let failed_http = registry_http_with_listing(
            b"tampered sequence-ratchet artifact",
            &shasum,
            r#"{"sequence":101,"valid_until":"2999-01-01T00:00:00Z","versions":[{"version":"0.5.0","protocols":["1"]}]}"#,
        );

        let error = resolve_single_config_with_http(dir.path(), &config, &failed_http).unwrap_err();
        assert!(error.contains("SHA256 mismatch"), "{error}");
        assert_eq!(
            saved_registry_ratchets(dir.path()).sequence.value(),
            Some(101),
            "the accepted sequence observation must reach disk before the shasum check"
        );
        assert_eq!(
            saved_registry_sequence_anchor(dir.path()),
            RegistrySequenceAnchor::Established(100),
            "the failed resolve must not advance the established anchor"
        );

        let rollback_http = registry_http_with_listing(
            EXPECTED_ARTIFACT,
            &shasum,
            r#"{"sequence":99,"valid_until":"2999-01-01T00:00:00Z","versions":[{"version":"0.5.0","protocols":["1"]}]}"#,
        );
        let error =
            resolve_single_config_with_http(dir.path(), &config, &rollback_http).unwrap_err();

        assert!(error.contains("sequence rollback"), "{error}");
        assert!(error.contains("previous 100, got 99"), "{error}");
        assert!(!rollback_http.was_requested("/0.5.0/download"));
    }

    #[test]
    fn registry_ratchet_sequence_survives_signature_bundle_failure() {
        let dir = tempfile::tempdir().unwrap();
        let config = provider_config("carina-rs/aws", None);
        let baseline_http = signed_registry_http(SIGNED_FIXTURE_BUNDLE, 200).json(
            REGISTRY_VERSIONS_URL,
            r#"{"sequence":100,"valid_until":"2999-01-01T00:00:00Z","versions":[{"version":"0.5.0","protocols":["1"]}]}"#,
        );
        resolve_single_config_with_http(dir.path(), &config, &baseline_http).unwrap();

        let failed_http = signed_registry_http(b"bundle unavailable", 503).json(
            REGISTRY_VERSIONS_URL,
            r#"{"sequence":101,"valid_until":"2999-01-01T00:00:00Z","versions":[{"version":"0.5.0","protocols":["1"]}]}"#,
        );

        let error = resolve_single_config_with_http(dir.path(), &config, &failed_http).unwrap_err();
        assert!(error.contains("HTTP 503"), "{error}");
        assert_eq!(
            saved_registry_ratchets(dir.path()).sequence.value(),
            Some(101),
            "the accepted sequence observation must reach disk before the bundle fetch"
        );
        assert_eq!(
            saved_registry_sequence_anchor(dir.path()),
            RegistrySequenceAnchor::Established(100),
            "the bundle failure must not advance the established anchor"
        );

        let shasum = sha256_bytes(SIGNED_FIXTURE_ARTIFACT);
        let rollback_http = registry_http_with_listing(
            SIGNED_FIXTURE_ARTIFACT,
            &shasum,
            r#"{"sequence":99,"valid_until":"2999-01-01T00:00:00Z","versions":[{"version":"0.5.0","protocols":["1"]}]}"#,
        );
        let error =
            resolve_single_config_with_http(dir.path(), &config, &rollback_http).unwrap_err();

        assert!(error.contains("sequence rollback"), "{error}");
        assert!(error.contains("previous 100, got 99"), "{error}");
    }

    #[test]
    fn registry_ratchet_refused_sequence_is_not_persisted() {
        const ARTIFACT: &[u8] = b"sequence refusal artifact";
        let dir = tempfile::tempdir().unwrap();
        let shasum = sha256_bytes(ARTIFACT);
        let config = provider_config("carina-rs/aws", None);
        let baseline_http = registry_http_with_listing(
            ARTIFACT,
            &shasum,
            r#"{"sequence":100,"valid_until":"2999-01-01T00:00:00Z","versions":[{"version":"0.5.0","protocols":["1"]}]}"#,
        );
        resolve_single_config_with_http(dir.path(), &config, &baseline_http).unwrap();

        let hostile_sequence = 100 + MAX_SEQUENCE_FAST_FORWARD + 1;
        let hostile_listing = format!(
            r#"{{"sequence":{hostile_sequence},"valid_until":"2999-01-01T00:00:00Z","versions":[{{"version":"0.5.0","protocols":["1"]}}]}}"#
        );
        let hostile_http = registry_http_with_listing(ARTIFACT, &shasum, &hostile_listing);
        let error =
            resolve_single_config_with_http(dir.path(), &config, &hostile_http).unwrap_err();
        assert!(error.contains("sequence fast-forward"), "{error}");

        let lock_path = dir.path().join("carina-providers.lock");
        let lock_file = LockFile::load(&lock_path).unwrap().unwrap();
        let registry = lock_file
            .find_by_source("carina-rs/aws")
            .unwrap()
            .registry
            .as_ref()
            .unwrap();
        assert_eq!(registry.sequence.value(), Some(100));

        let legitimate_http = registry_http_with_listing(
            ARTIFACT,
            &shasum,
            r#"{"sequence":101,"valid_until":"2999-01-01T00:00:00Z","versions":[{"version":"0.5.0","protocols":["1"]}]}"#,
        );
        resolve_single_config_with_http(dir.path(), &config, &legitimate_http).unwrap();
        let lock_file = LockFile::load(&lock_path).unwrap().unwrap();
        let registry = lock_file
            .find_by_source("carina-rs/aws")
            .unwrap()
            .registry
            .as_ref()
            .unwrap();
        assert_eq!(registry.sequence.value(), Some(101));
    }

    #[test]
    fn registry_security_rejected_listing_persists_no_observations() {
        const ARTIFACT: &[u8] = b"atomic listing-ratchet artifact";
        let dir = tempfile::tempdir().unwrap();
        let shasum = sha256_bytes(ARTIFACT);
        let config = provider_config("carina-rs/aws", None);
        let baseline_http = registry_http_with_listing(
            ARTIFACT,
            &shasum,
            r#"{"sequence":100,"versions":[{"version":"0.5.0","protocols":["1"]}]}"#,
        );
        resolve_single_config_with_http(dir.path(), &config, &baseline_http).unwrap();

        let rollback_with_valid_until = registry_http_with_listing(
            ARTIFACT,
            &shasum,
            r#"{"sequence":5,"valid_until":"2999-01-01T00:00:00Z","versions":[{"version":"0.5.0","protocols":["1"]}]}"#,
        );
        let error =
            resolve_single_config_with_http(dir.path(), &config, &rollback_with_valid_until)
                .unwrap_err();
        assert!(error.contains("sequence rollback"), "{error}");

        let lock_path = dir.path().join("carina-providers.lock");
        let lock_file = LockFile::load(&lock_path).unwrap().unwrap();
        let registry = lock_file
            .find_by_source("carina-rs/aws")
            .unwrap()
            .registry
            .as_ref()
            .unwrap();
        assert_eq!(registry.sequence.value(), Some(100));
        assert!(
            !registry.valid_until_present,
            "a listing rejected for rollback must not promote another observation"
        );

        let accepted_sequence_missing_valid_until = registry_http_with_listing(
            ARTIFACT,
            &shasum,
            r#"{"sequence":101,"versions":[{"version":"0.5.0","protocols":["1"]}]}"#,
        );
        resolve_single_config_with_http(
            dir.path(),
            &config,
            &accepted_sequence_missing_valid_until,
        )
        .expect("the rejected listing must not install valid_until presence");

        let lock_file = LockFile::load(&lock_path).unwrap().unwrap();
        let registry = lock_file
            .find_by_source("carina-rs/aws")
            .unwrap()
            .registry
            .as_ref()
            .unwrap();
        assert_eq!(
            registry.sequence.value(),
            Some(101),
            "the later fully valid listing must establish the next sequence"
        );
        assert!(!registry.valid_until_present);
    }

    #[test]
    fn registry_ratchet_valid_until_presence_survives_failed_resolve() {
        const EXPECTED_ARTIFACT: &[u8] = b"expected valid-until artifact";
        let dir = tempfile::tempdir().unwrap();
        let shasum = sha256_bytes(EXPECTED_ARTIFACT);
        let initial_http = registry_http_with_listing(
            b"tampered valid-until artifact",
            &shasum,
            r#"{"valid_until":"2999-01-01T00:00:00Z","versions":[{"version":"0.5.0","protocols":["1"]}]}"#,
        );

        let error = resolve_single_config_with_http(
            dir.path(),
            &provider_config("carina-rs/aws", None),
            &initial_http,
        )
        .unwrap_err();
        assert!(error.contains("SHA256 mismatch"), "{error}");
        assert!(
            saved_lock_contents(dir.path()).contains("valid_until_present = true"),
            "valid_until presence must be durable before the shasum check"
        );

        let missing_http = registry_http_with_listing(
            EXPECTED_ARTIFACT,
            &shasum,
            r#"{"versions":[{"version":"0.5.0","protocols":["1"]}]}"#,
        );
        let error = resolve_single_config_with_http(
            dir.path(),
            &provider_config("carina-rs/aws", None),
            &missing_http,
        )
        .unwrap_err();
        assert!(error.contains("valid_until field disappeared"), "{error}");
        assert!(
            saved_lock_contents(dir.path()).contains("valid_until_present = true"),
            "a missing field must not demote the durable presence ratchet"
        );
    }

    #[test]
    fn registry_ratchet_malformed_valid_until_is_not_recorded() {
        const ARTIFACT: &[u8] = b"malformed valid-until artifact";
        let dir = tempfile::tempdir().unwrap();
        let shasum = sha256_bytes(ARTIFACT);
        let malformed_http = registry_http_with_listing(
            ARTIFACT,
            &shasum,
            r#"{"valid_until":"not-a-timestamp","versions":[{"version":"0.5.0","protocols":["1"]}]}"#,
        );

        let error = resolve_single_config_with_http(
            dir.path(),
            &provider_config("carina-rs/aws", None),
            &malformed_http,
        )
        .unwrap_err();
        assert!(
            error.contains("Invalid registry valid_until timestamp"),
            "{error}"
        );

        let lock_path = dir.path().join("carina-providers.lock");
        let lock_file = LockFile::load(&lock_path).unwrap().unwrap_or_default();
        let ratchets = lock_file.known_registry_ratchets("carina-rs/aws").unwrap();
        assert!(
            !ratchets.valid_until_present,
            "a malformed timestamp is not an accepted presence observation"
        );
    }

    #[test]
    fn registry_malformed_valid_until_does_not_poison_next_listing() {
        const ARTIFACT: &[u8] = b"valid-until recovery artifact";
        let dir = tempfile::tempdir().unwrap();
        let shasum = sha256_bytes(ARTIFACT);
        let config = provider_config("carina-rs/aws", None);
        let malformed_http = registry_http_with_listing(
            ARTIFACT,
            &shasum,
            r#"{"valid_until":"not-a-timestamp","versions":[{"version":"0.5.0","protocols":["1"]}]}"#,
        );

        let error =
            resolve_single_config_with_http(dir.path(), &config, &malformed_http).unwrap_err();
        assert!(
            error.contains("Invalid registry valid_until timestamp"),
            "{error}"
        );

        let honest_http = registry_http_with_listing(
            ARTIFACT,
            &shasum,
            r#"{"versions":[{"version":"0.5.0","protocols":["1"]}]}"#,
        );
        let path = resolve_single_config_with_http(dir.path(), &config, &honest_http)
            .expect("the malformed timestamp must not poison the next listing");
        assert_eq!(fs::read(path).unwrap(), ARTIFACT);
    }

    #[test]
    fn registry_security_expired_valid_until_is_not_recorded() {
        const ARTIFACT: &[u8] = b"expired valid-until artifact";
        let dir = tempfile::tempdir().unwrap();
        let shasum = sha256_bytes(ARTIFACT);
        let expired_http = registry_http_with_listing(
            ARTIFACT,
            &shasum,
            r#"{"valid_until":"2000-01-01T00:00:00Z","versions":[{"version":"0.5.0","protocols":["1"]}]}"#,
        );

        let error = resolve_single_config_with_http(
            dir.path(),
            &provider_config("carina-rs/aws", None),
            &expired_http,
        )
        .unwrap_err();
        assert!(error.contains("valid_until is expired"), "{error}");
        assert!(
            !saved_registry_ratchets(dir.path()).valid_until_present,
            "an expired listing must not install valid_until presence"
        );

        let honest_http = registry_http_with_listing(
            ARTIFACT,
            &shasum,
            r#"{"versions":[{"version":"0.5.0","protocols":["1"]}]}"#,
        );
        resolve_single_config_with_http(
            dir.path(),
            &provider_config("carina-rs/aws", None),
            &honest_http,
        )
        .expect("the expired listing must not poison an honest listing without valid_until");
    }

    #[test]
    fn registry_ratchet_transparency_log_presence_survives_failed_resolve() {
        const EXPECTED_ARTIFACT: &[u8] = b"expected transparency-log artifact";
        let dir = tempfile::tempdir().unwrap();
        let shasum = sha256_bytes(EXPECTED_ARTIFACT);
        let download = format!(
            r#"{{"download_url":"https://downloads.example.test/aws.wasm","shasum":"{shasum}","transparency_log":{{"observed":true}}}}"#
        );
        let initial_http = registry_http_with_listing(
            b"tampered transparency-log artifact",
            &shasum,
            r#"{"versions":[{"version":"0.5.0","protocols":["1"]}]}"#,
        )
        .json(REGISTRY_DOWNLOAD_URL, &download);

        let error = resolve_single_config_with_http(
            dir.path(),
            &provider_config("carina-rs/aws", None),
            &initial_http,
        )
        .unwrap_err();
        assert!(error.contains("SHA256 mismatch"), "{error}");
        assert!(
            saved_lock_contents(dir.path()).contains("transparency_log_present = true"),
            "transparency-log presence must be durable before the shasum check"
        );

        let missing_http = registry_http_with_listing(
            EXPECTED_ARTIFACT,
            &shasum,
            r#"{"versions":[{"version":"0.5.0","protocols":["1"]}]}"#,
        );
        let error = resolve_single_config_with_http(
            dir.path(),
            &provider_config("carina-rs/aws", None),
            &missing_http,
        )
        .unwrap_err();
        assert!(
            error.contains("transparency_log field disappeared"),
            "{error}"
        );
        assert!(
            saved_lock_contents(dir.path()).contains("transparency_log_present = true"),
            "a missing field must not demote the durable presence ratchet"
        );
    }

    #[test]
    fn registry_security_rejected_download_pin_does_not_promote_transparency_log() {
        const ARTIFACT: &[u8] = b"transparency-log validation artifact";
        let dir = tempfile::tempdir().unwrap();
        let config = provider_config("carina-rs/aws", None);
        let shasum = sha256_bytes(ARTIFACT);
        let baseline_http = registry_http_with_listing(
            ARTIFACT,
            &shasum,
            r#"{"sequence":10,"versions":[{"version":"0.5.0","protocols":["1"]}]}"#,
        );
        resolve_single_config_with_http(dir.path(), &config, &baseline_http).unwrap();

        let hostile_shasum = sha256_bytes(b"hostile transparency-log shasum");
        let hostile_download = format!(
            r#"{{"download_url":"https://downloads.example.test/aws.wasm","shasum":"{hostile_shasum}","transparency_log":{{"attacker":true}}}}"#
        );
        let hostile_http = registry_http_with_listing(
            ARTIFACT,
            &hostile_shasum,
            r#"{"sequence":11,"versions":[{"version":"0.5.0","protocols":["1"]}]}"#,
        )
        .json(REGISTRY_DOWNLOAD_URL, &hostile_download);
        let error =
            resolve_single_config_with_http(dir.path(), &config, &hostile_http).unwrap_err();
        assert!(error.contains("shasum pin mismatch"), "{error}");
        let ratchets = saved_registry_ratchets(dir.path());
        assert_eq!(ratchets.sequence.value(), Some(11));
        assert!(
            !ratchets.transparency_log_present,
            "a download response rejected by lock-pin validation must not promote presence"
        );

        let honest_http = registry_http_with_listing(
            ARTIFACT,
            &shasum,
            r#"{"sequence":11,"versions":[{"version":"0.5.0","protocols":["1"]}]}"#,
        );
        let path = resolve_single_config_with_http(dir.path(), &config, &honest_http)
            .expect("the rejected download response must not poison the honest registry");
        assert_eq!(fs::read(path).unwrap(), ARTIFACT);
    }

    #[test]
    fn registry_ratchet_verified_signature_is_durable_without_caller_save() {
        let dir = tempfile::tempdir().unwrap();
        let mut lock_file = LockFile::default();
        resolve_provider_with_http(
            dir.path(),
            "carina-rs/aws",
            "0.5.0",
            "aws",
            &mut lock_file,
            &signed_registry_http(SIGNED_FIXTURE_BUNDLE, 200),
        )
        .unwrap();

        let lock_contents = saved_lock_contents(dir.path());
        assert!(lock_contents.contains(SIGNED_FIXTURE_IDENTITY));
        drop(lock_file);

        let lock_path = dir.path().join("carina-providers.lock");
        let mut reloaded = LockFile::load(&lock_path).unwrap().unwrap();
        let shasum = sha256_bytes(SIGNED_FIXTURE_ARTIFACT);
        let unsigned_http = registry_http(SIGNED_FIXTURE_ARTIFACT, &shasum);
        let error = resolve_provider_with_http(
            dir.path(),
            "carina-rs/aws",
            "0.5.0",
            "aws",
            &mut reloaded,
            &unsigned_http,
        )
        .unwrap_err();

        assert!(error.contains("signature"), "{error}");
        assert!(error.contains("signatures as required"), "{error}");
    }

    #[test]
    fn registry_sequence_observation_is_durable_from_version_selection_entry_point() {
        const ARTIFACT: &[u8] = b"version-selection ratchet artifact";
        let dir = tempfile::tempdir().unwrap();
        let shasum = sha256_bytes(ARTIFACT);
        let source = match parse_provider_source("carina-rs/aws").unwrap() {
            ProviderSource::Registry(source) => source,
            ProviderSource::GithubDirect { .. } => unreachable!(),
        };
        let config = provider_config("carina-rs/aws", None);
        let lock_path = dir.path().join("carina-providers.lock");
        let mut lock_file = LockFile::default();
        let initial_http = registry_http_with_listing(
            ARTIFACT,
            &shasum,
            r#"{"sequence":100,"versions":[{"version":"0.5.0","protocols":["1"]}]}"#,
        );

        let selected = resolve_registry_version_with_http(
            &source,
            &config,
            &mut lock_file,
            &lock_path,
            &initial_http,
        )
        .unwrap();
        assert_eq!(selected, "0.5.0");
        drop(lock_file);
        assert!(saved_lock_contents(dir.path()).contains("sequence = 100"));
        assert_eq!(
            saved_registry_sequence_anchor(dir.path()),
            RegistrySequenceAnchor::Unestablished,
            "version selection alone must not establish an anti-rollback anchor"
        );

        let mut reloaded = LockFile::load(&lock_path).unwrap().unwrap();
        let rollback_http = registry_http_with_listing(
            ARTIFACT,
            &shasum,
            r#"{"sequence":5,"versions":[{"version":"0.5.0","protocols":["1"]}]}"#,
        );
        let selected = resolve_registry_version_with_http(
            &source,
            &config,
            &mut reloaded,
            &lock_path,
            &rollback_http,
        )
        .expect("an unpinned observation must not become a rollback floor");

        assert_eq!(selected, "0.5.0");
        assert_eq!(
            saved_registry_ratchets(dir.path()).sequence.value(),
            Some(100),
            "the lower accepted observation must not erase the durable maximum"
        );
        assert_eq!(
            saved_registry_sequence_anchor(dir.path()),
            RegistrySequenceAnchor::Unestablished
        );
    }

    #[test]
    fn registry_source_warns_but_resolves_lock_pinned_yanked_version() {
        const CHILD_ENV: &str = "CARINA_TEST_YANKED_LOCK_PIN_WARNING";
        const TEST_NAME: &str = "provider_resolver::tests::registry_source_warns_but_resolves_lock_pinned_yanked_version";
        const ARTIFACT: &[u8] = b"lock-pinned yanked provider";

        if std::env::var_os(CHILD_ENV).is_some() {
            let dir = tempfile::tempdir().unwrap();
            let shasum = sha256_bytes(ARTIFACT);
            let mut lock_file = LockFile::default();
            lock_file.upsert(LockEntry {
                name: "awscc".into(),
                source: "carina-rs/aws".into(),
                kind: LockEntryKind::Version {
                    version: "0.5.0".into(),
                    constraint: None,
                },
                sha256: shasum.clone(),
                registry: None,
            });
            lock_file
                .save(&dir.path().join("carina-providers.lock"))
                .unwrap();
            let versions_url = "https://registry.carina-rs.dev/v1/providers/carina-rs/aws/versions";
            let http = registry_http(ARTIFACT, &shasum).json(
                versions_url,
                r#"{"sequence":7,"valid_until":"2999-01-01T00:00:00Z","versions":[{"version":"0.5.0","protocols":["1"],"yanked":true}]}"#,
            );

            let path = resolve_single_config_with_http(
                dir.path(),
                &provider_config("carina-rs/aws", None),
                &http,
            )
            .unwrap();

            assert_eq!(fs::read(path).unwrap(), ARTIFACT);
            assert_eq!(
                http.request_count(versions_url),
                1,
                "lock reuse must not fetch the listing before the pinned provider path"
            );
            return;
        }

        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([TEST_NAME, "--exact", "--nocapture"])
            .env(CHILD_ENV, "1")
            .output()
            .unwrap();
        assert!(output.status.success(), "child test failed: {output:?}");
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(stderr.contains("warning"), "{stderr}");
        assert!(stderr.contains("carina-rs/aws@0.5.0"), "{stderr}");
        assert!(stderr.contains("yanked"), "{stderr}");
        assert!(stderr.contains("carina-providers.lock"), "{stderr}");
    }

    #[test]
    fn registry_source_refuses_to_newly_pin_exact_yanked_version() {
        const ARTIFACT: &[u8] = b"new yanked provider pin";
        let dir = tempfile::tempdir().unwrap();
        let shasum = sha256_bytes(ARTIFACT);
        let http = registry_http(ARTIFACT, &shasum).json(
            "https://registry.carina-rs.dev/v1/providers/carina-rs/aws/versions",
            r#"{"sequence":7,"valid_until":"2999-01-01T00:00:00Z","versions":[{"version":"0.5.0","protocols":["1"],"yanked":true}]}"#,
        );
        let mut lock_file = LockFile::default();

        let error = resolve_provider_with_http(
            dir.path(),
            "carina-rs/aws",
            "0.5.0",
            "aws",
            &mut lock_file,
            &http,
        )
        .unwrap_err();

        assert!(error.contains("0.5.0"), "{error}");
        assert!(error.contains("yanked"), "{error}");
        assert!(lock_file.find_by_source("carina-rs/aws").is_none());
        assert!(!http.was_requested("https://downloads.example.test/aws.wasm"));
    }

    #[test]
    fn registry_yank_ratchet_survives_a_refused_resolve() {
        const ARTIFACT: &[u8] = b"strip after refusal";
        let dir = tempfile::tempdir().unwrap();
        let shasum = sha256_bytes(ARTIFACT);
        let versions_url = "https://registry.carina-rs.dev/v1/providers/carina-rs/aws/versions";
        let download_040 = format!(
            r#"{{"protocols":["1"],"filename":"aws.wasm","download_url":"https://downloads.example.test/aws.wasm","shasum":"{shasum}","shasum_authored_by":"registry"}}"#
        );

        let mut lock_round1 = LockFile::default();
        let yanked_http = registry_http(ARTIFACT, &shasum)
            .json(
                versions_url,
                r#"{"sequence":7,"valid_until":"2999-01-01T00:00:00Z","versions":[{"version":"0.4.0","protocols":["1"],"yanked":true}]}"#,
            )
            .json(
                "https://registry.carina-rs.dev/v1/providers/carina-rs/aws/0.4.0/download",
                &download_040,
            );
        let error = resolve_provider_with_http(
            dir.path(),
            "carina-rs/aws",
            "0.4.0",
            "aws",
            &mut lock_round1,
            &yanked_http,
        )
        .unwrap_err();
        assert!(error.contains("yanked"), "round 1 should refuse: {error}");

        let lock_path = dir.path().join("carina-providers.lock");
        let mut lock_round2 = LockFile::load(&lock_path).unwrap().unwrap_or_default();
        let stripped_http = registry_http(ARTIFACT, &shasum)
            .json(
                versions_url,
                r#"{"sequence":8,"valid_until":"2999-01-01T00:00:00Z","versions":[{"version":"0.4.0","protocols":["1"]}]}"#,
            )
            .json(
                "https://registry.carina-rs.dev/v1/providers/carina-rs/aws/0.4.0/download",
                &download_040,
            );
        let result = resolve_provider_with_http(
            dir.path(),
            "carina-rs/aws",
            "0.4.0",
            "aws",
            &mut lock_round2,
            &stripped_http,
        );

        assert!(
            result.is_err(),
            "yank ratchet was lost: withdrawn 0.4.0 pinned after stripped retry"
        );
    }

    #[test]
    fn registry_source_records_yanks_stickily_per_version() {
        const ARTIFACT: &[u8] = b"sticky yank registry provider";
        let dir = tempfile::tempdir().unwrap();
        let shasum = sha256_bytes(ARTIFACT);
        let versions_url = "https://registry.carina-rs.dev/v1/providers/carina-rs/aws/versions";
        let mut lock_file = LockFile::default();
        let first_http = registry_http(ARTIFACT, &shasum).json(
            versions_url,
            r#"{"sequence":7,"valid_until":"2999-01-01T00:00:00Z","versions":[{"version":"0.5.0","protocols":["1"]},{"version":"0.4.0","protocols":["1"],"yanked":true}]}"#,
        );

        resolve_provider_with_http(
            dir.path(),
            "carina-rs/aws",
            "0.5.0",
            "aws",
            &mut lock_file,
            &first_http,
        )
        .unwrap();

        let recorded = lock_file
            .find_by_source("carina-rs/aws")
            .unwrap()
            .registry
            .as_ref()
            .unwrap()
            .yanked_versions();
        assert!(recorded.contains("0.4.0"));
        assert!(!recorded.contains("0.5.0"));

        let second_http = registry_http(ARTIFACT, &shasum).json(
            versions_url,
            r#"{"sequence":8,"valid_until":"2999-01-01T00:00:00Z","versions":[{"version":"0.5.0","protocols":["1"]},{"version":"0.4.0","protocols":["1"],"yanked":true},{"version":"0.3.0","protocols":["1"],"yanked":true}]}"#,
        );
        resolve_provider_with_http(
            dir.path(),
            "carina-rs/aws",
            "0.5.0",
            "aws",
            &mut lock_file,
            &second_http,
        )
        .unwrap();

        let lock_path = dir.path().join("carina-providers.lock");
        lock_file.save(&lock_path).unwrap();
        let mut lock_file = LockFile::load(&lock_path).unwrap().unwrap();
        let recorded = lock_file
            .find_by_source("carina-rs/aws")
            .unwrap()
            .registry
            .as_ref()
            .unwrap()
            .yanked_versions();
        assert!(recorded.contains("0.4.0"));
        assert!(recorded.contains("0.3.0"));

        let stripped_http = registry_http(ARTIFACT, &shasum).json(
            versions_url,
            r#"{"sequence":9,"valid_until":"2999-01-01T00:00:00Z","versions":[{"version":"0.5.0","protocols":["1"]},{"version":"0.4.0","protocols":["1"]},{"version":"0.3.0","protocols":["1"],"yanked":true}]}"#,
        );
        let error = resolve_provider_with_http(
            dir.path(),
            "carina-rs/aws",
            "0.5.0",
            "aws",
            &mut lock_file,
            &stripped_http,
        )
        .unwrap_err();

        assert!(error.contains("0.4.0"), "{error}");
        assert!(error.contains("yanked"), "{error}");
        assert!(!stripped_http.was_requested("/0.5.0/download"));
    }

    #[test]
    fn registry_yank_observation_survives_between_selection_and_pin_fetches() {
        const ARTIFACT: &[u8] = b"selection-to-pin yank ratchet";
        let dir = tempfile::tempdir().unwrap();
        let shasum = sha256_bytes(ARTIFACT);
        let versions_url = "https://registry.carina-rs.dev/v1/providers/carina-rs/aws/versions";
        let source = match parse_provider_source("carina-rs/aws").unwrap() {
            ProviderSource::Registry(source) => source,
            ProviderSource::GithubDirect { .. } => unreachable!(),
        };
        let config = provider_config("carina-rs/aws", None);
        let mut lock_file = LockFile::default();
        let selection_http = registry_http(ARTIFACT, &shasum).json(
            versions_url,
            r#"{"sequence":7,"valid_until":"2999-01-01T00:00:00Z","versions":[{"version":"0.5.0","protocols":["1"]},{"version":"0.4.0","protocols":["1"],"yanked":true}]}"#,
        );

        let lock_path = dir.path().join("carina-providers.lock");
        let selected = resolve_registry_version_with_http(
            &source,
            &config,
            &mut lock_file,
            &lock_path,
            &selection_http,
        )
        .unwrap();
        assert_eq!(selected, "0.5.0");

        let stripped_pin_http = registry_http(ARTIFACT, &shasum).json(
            versions_url,
            r#"{"sequence":8,"valid_until":"2999-01-01T00:00:00Z","versions":[{"version":"0.5.0","protocols":["1"]},{"version":"0.4.0","protocols":["1"]}]}"#,
        );
        let error = resolve_provider_with_http(
            dir.path(),
            "carina-rs/aws",
            &selected,
            "aws",
            &mut lock_file,
            &stripped_pin_http,
        )
        .unwrap_err();

        assert!(error.contains("0.4.0"), "{error}");
        assert!(error.contains("yanked"), "{error}");
        assert!(!stripped_pin_http.was_requested("/0.5.0/download"));
    }

    #[test]
    fn registry_source_verifies_signature_and_pins_identity() {
        let dir = tempfile::tempdir().unwrap();
        let http = signed_registry_http(SIGNED_FIXTURE_BUNDLE, 200);
        let mut lock_file = LockFile::default();

        let path = resolve_provider_with_http(
            dir.path(),
            "carina-rs/aws",
            "0.5.0",
            "aws",
            &mut lock_file,
            &http,
        )
        .unwrap();

        assert_eq!(fs::read(path).unwrap(), SIGNED_FIXTURE_ARTIFACT);
        let registry = lock_file
            .find_by_source("carina-rs/aws")
            .unwrap()
            .registry
            .as_ref()
            .unwrap();
        let RegistrySignatureProtection::RequiredPinned(pin) = &registry.signature else {
            panic!("verified signature must carry its identity pin");
        };
        assert_eq!(pin.certificate_identity, SIGNED_FIXTURE_IDENTITY);
        assert_eq!(pin.certificate_oidc_issuer, SIGNED_FIXTURE_ISSUER);

        let lock_path = dir.path().join("carina-providers.lock");
        lock_file.save(&lock_path).unwrap();
        let lock_contents = fs::read_to_string(lock_path).unwrap();
        assert!(
            lock_contents.contains(&format!(
                "certificate_identity = {SIGNED_FIXTURE_IDENTITY:?}"
            )),
            "{lock_contents}"
        );
        assert!(
            lock_contents.contains(&format!(
                "certificate_oidc_issuer = {SIGNED_FIXTURE_ISSUER:?}"
            )),
            "{lock_contents}"
        );
    }

    #[test]
    fn registry_source_prints_first_use_pin_notice_once() {
        const CHILD_ENV: &str = "CARINA_TEST_FIRST_USE_PIN_NOTICE";
        const TEST_NAME: &str =
            "provider_resolver::tests::registry_source_prints_first_use_pin_notice_once";

        if std::env::var_os(CHILD_ENV).is_some() {
            let dir = tempfile::tempdir().unwrap();
            let http = signed_registry_http(SIGNED_FIXTURE_BUNDLE, 200);
            let mut lock_file = LockFile::default();
            resolve_provider_with_http(
                dir.path(),
                "carina-rs/aws",
                "0.5.0",
                "aws",
                &mut lock_file,
                &http,
            )
            .unwrap();
            resolve_provider_with_http(
                dir.path(),
                "carina-rs/aws",
                "0.5.0",
                "aws",
                &mut lock_file,
                &http,
            )
            .unwrap();
            return;
        }

        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([TEST_NAME, "--exact", "--nocapture"])
            .env(CHILD_ENV, "1")
            .output()
            .unwrap();
        assert!(output.status.success(), "child test failed: {output:?}");
        let stderr = String::from_utf8(output.stderr).unwrap();
        let notice = format!(
            "carina: pinned signing identity for carina-rs/aws: {SIGNED_FIXTURE_IDENTITY} (issuer {SIGNED_FIXTURE_ISSUER})"
        );
        assert_eq!(stderr.matches(&notice).count(), 1, "{stderr}");
    }

    #[test]
    fn registry_source_reverifies_cached_artifact_against_pinned_identity() {
        let dir = tempfile::tempdir().unwrap();
        let http = signed_registry_http(SIGNED_FIXTURE_BUNDLE, 200);
        let mut lock_file = LockFile::default();

        resolve_provider_with_http(
            dir.path(),
            "carina-rs/aws",
            "0.5.0",
            "aws",
            &mut lock_file,
            &http,
        )
        .unwrap();
        resolve_provider_with_http(
            dir.path(),
            "carina-rs/aws",
            "0.5.0",
            "aws",
            &mut lock_file,
            &http,
        )
        .unwrap();

        assert_eq!(http.request_count(SIGNED_FIXTURE_BUNDLE_URL), 2);
        assert_eq!(
            http.request_count("https://downloads.example.test/aws.wasm"),
            1
        );
    }

    #[test]
    fn registry_source_rejects_declared_identity_mismatch_before_artifact_download() {
        let initial_dir = tempfile::tempdir().unwrap();
        let initial_http = signed_registry_http(SIGNED_FIXTURE_BUNDLE, 200);
        let mut lock_file = LockFile::default();
        resolve_provider_with_http(
            initial_dir.path(),
            "carina-rs/aws",
            "0.5.0",
            "aws",
            &mut lock_file,
            &initial_http,
        )
        .unwrap();
        let pinned_dir = tempfile::tempdir().unwrap();
        let lock_path = pinned_dir.path().join("carina-providers.lock");
        lock_file.save(&lock_path).unwrap();
        let mut lock_file = LockFile::load(&lock_path).unwrap().unwrap();

        let shasum = sha256_bytes(SIGNED_FIXTURE_ARTIFACT);
        let mismatched_http = signed_registry_http(SIGNED_FIXTURE_BUNDLE, 200).json(
            "https://registry.carina-rs.dev/v1/providers/carina-rs/aws/0.5.0/download",
            &format!(
                r#"{{"download_url":"https://downloads.example.test/aws.wasm","shasum":"{shasum}","signature":{{"type":"sigstore-bundle","certificate_identity":"https://github.com/example/other/.github/workflows/release.yml@refs/heads/main","certificate_oidc_issuer":"{SIGNED_FIXTURE_ISSUER}","bundle_url":"{SIGNED_FIXTURE_BUNDLE_URL}"}}}}"#
            ),
        );

        let error = resolve_provider_with_http(
            pinned_dir.path(),
            "carina-rs/aws",
            "0.5.0",
            "aws",
            &mut lock_file,
            &mismatched_http,
        )
        .unwrap_err();

        assert!(error.contains("identity"), "{error}");
        assert!(error.contains("no override"), "{error}");
        assert!(error.contains("verifying out-of-band"), "{error}");
        assert!(
            error.contains("carina providers repin-identity carina-rs/aws"),
            "{error}"
        );
        assert!(!mismatched_http.was_requested("https://downloads.example.test/aws.wasm"));
        assert!(!mismatched_http.was_requested(SIGNED_FIXTURE_BUNDLE_URL));
    }

    #[test]
    fn registry_source_rejects_signature_downgrade_when_identity_is_pinned() {
        let dir = tempfile::tempdir().unwrap();
        let initial_http = signed_registry_http(SIGNED_FIXTURE_BUNDLE, 200);
        let mut lock_file = LockFile::default();
        resolve_provider_with_http(
            dir.path(),
            "carina-rs/aws",
            "0.5.0",
            "aws",
            &mut lock_file,
            &initial_http,
        )
        .unwrap();
        let shasum = sha256_bytes(SIGNED_FIXTURE_ARTIFACT);
        let unsigned_http = signed_registry_http(SIGNED_FIXTURE_BUNDLE, 200).json(
            "https://registry.carina-rs.dev/v1/providers/carina-rs/aws/0.5.0/download",
            &format!(
                r#"{{"download_url":"https://downloads.example.test/aws.wasm","shasum":"{shasum}"}}"#
            ),
        );

        let error = resolve_provider_with_http(
            dir.path(),
            "carina-rs/aws",
            "0.5.0",
            "aws",
            &mut lock_file,
            &unsigned_http,
        )
        .unwrap_err();

        assert!(error.contains("signature"), "{error}");
        assert!(error.contains("no override"), "{error}");
        assert!(error.contains("signatures as required"), "{error}");
        assert!(error.contains("verifying out-of-band"), "{error}");
        assert!(
            error.contains("carina providers repin-identity carina-rs/aws"),
            "{error}"
        );
        assert!(!unsigned_http.was_requested("https://downloads.example.test/aws.wasm"));
    }

    #[test]
    fn registry_source_reuses_existing_signature_identity_pin() {
        let dir = tempfile::tempdir().unwrap();
        let shasum = sha256_bytes(SIGNED_FIXTURE_ARTIFACT);
        let mut lock_file = LockFile::default();
        lock_file
            .upsert_registry(
                LockEntry {
                    name: "aws".into(),
                    source: "carina-rs/aws".into(),
                    kind: LockEntryKind::Version {
                        version: "0.5.0".into(),
                        constraint: None,
                    },
                    sha256: shasum,
                    registry: Some(RegistryLock {
                        resolved_hostname: "registry.carina-rs.dev".into(),
                        sequence: RegistrySequence::Present(7),
                        sequence_anchor: RegistrySequenceAnchor::Established(7),
                        valid_until_present: true,
                        yanked_versions: YankedRegistryVersions::default(),
                        signature: signed_fixture_pin(),
                        transparency_log_present: false,
                    }),
                },
                registry_host_lock("https://registry.carina-rs.dev/v1/providers/"),
            )
            .unwrap();
        let http = signed_registry_http(SIGNED_FIXTURE_BUNDLE, 200);

        resolve_provider_with_http(
            dir.path(),
            "carina-rs/aws",
            "0.5.0",
            "aws",
            &mut lock_file,
            &http,
        )
        .unwrap();

        let registry = lock_file
            .find_by_source("carina-rs/aws")
            .unwrap()
            .registry
            .as_ref()
            .unwrap();
        let RegistrySignatureProtection::RequiredPinned(pin) = &registry.signature else {
            panic!("signed registry lock must retain its identity pin");
        };
        assert_eq!(pin.certificate_identity, SIGNED_FIXTURE_IDENTITY);
        assert_eq!(pin.certificate_oidc_issuer, SIGNED_FIXTURE_ISSUER);
    }

    #[test]
    fn registry_source_rejects_oversized_signature_bundle_and_retains_download() {
        let dir = tempfile::tempdir().unwrap();
        let oversized = vec![b' '; MAX_SIGNATURE_BUNDLE_BYTES + 1];
        let http = signed_registry_http(&oversized, 200);
        let mut lock_file = LockFile::default();

        let error = resolve_provider_with_http(
            dir.path(),
            "carina-rs/aws",
            "0.5.0",
            "aws",
            &mut lock_file,
            &http,
        )
        .unwrap_err();

        assert!(error.contains("exceeding"), "{error}");
        assert!(error.contains("no override"), "{error}");
        assert_eq!(
            fs::read(cache_path_wasm(dir.path(), "carina-rs/aws", "0.5.0")).unwrap(),
            SIGNED_FIXTURE_ARTIFACT
        );
    }

    #[test]
    fn registry_source_rejects_non_https_signature_bundle_url() {
        let dir = tempfile::tempdir().unwrap();
        let shasum = sha256_bytes(SIGNED_FIXTURE_ARTIFACT);
        let http = signed_registry_http(SIGNED_FIXTURE_BUNDLE, 200).json(
            "https://registry.carina-rs.dev/v1/providers/carina-rs/aws/0.5.0/download",
            &format!(
                r#"{{"download_url":"https://downloads.example.test/aws.wasm","shasum":"{shasum}","signature":{{"type":"sigstore-bundle","certificate_identity":"{SIGNED_FIXTURE_IDENTITY}","certificate_oidc_issuer":"{SIGNED_FIXTURE_ISSUER}","bundle_url":"http://downloads.example.test/aws.sigstore.json"}}}}"#
            ),
        );
        let mut lock_file = LockFile::default();

        let error = resolve_provider_with_http(
            dir.path(),
            "carina-rs/aws",
            "0.5.0",
            "aws",
            &mut lock_file,
            &http,
        )
        .unwrap_err();

        assert!(error.contains("HTTPS"), "{error}");
        assert!(error.contains("no override"), "{error}");
        assert_eq!(
            fs::read(cache_path_wasm(dir.path(), "carina-rs/aws", "0.5.0")).unwrap(),
            SIGNED_FIXTURE_ARTIFACT
        );
    }

    #[test]
    fn lock_load_rejects_partial_registry_identity_pin() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("carina-providers.lock");
        let one_sided_pin = format!(
            r#"version = 3

[registry_host."registry.carina-rs.dev"]
discovery_pin_present = true

[registry_host."registry.carina-rs.dev".discovery_values]
"providers.v1" = "https://registry.carina-rs.dev/v1/providers/"

[[provider]]
name = "aws"
source = "carina-rs/aws"
mode = "version"
version = "0.5.0"
sha256 = "abc"

[provider.registry]
resolved_hostname = "registry.carina-rs.dev"
sequence_present = true
sequence = 7
sequence_anchor_established = true
sequence_anchor = 7
valid_until_present = true
signature_present = true
certificate_identity = {SIGNED_FIXTURE_IDENTITY:?}
transparency_log_present = false
"#
        );

        let direct_error =
            LockFile::from_toml_str(&one_sided_pin, Path::new("carina-providers.lock"))
                .unwrap_err();
        assert!(
            direct_error.to_string().contains("inconsistent"),
            "{direct_error}"
        );
        fs::write(&lock_path, one_sided_pin).unwrap();

        let error = LockFile::load(&lock_path).unwrap_err();
        let rendered = error.to_string();
        assert!(rendered.contains("inconsistent"), "{error}");
        assert!(rendered.contains("certificate_identity"), "{error}");
    }

    #[test]
    fn registry_source_rejects_tampered_signature_bundle_and_removes_download() {
        let dir = tempfile::tempdir().unwrap();
        let mut bundle: serde_json::Value = serde_json::from_slice(SIGNED_FIXTURE_BUNDLE).unwrap();
        let encoded = bundle["verificationMaterial"]["tlogEntries"][0]["canonicalizedBody"]
            .as_str()
            .unwrap();
        let mut canonicalized_body = BASE64_STANDARD.decode(encoded).unwrap();
        canonicalized_body[0] ^= 1;
        bundle["verificationMaterial"]["tlogEntries"][0]["canonicalizedBody"] =
            serde_json::json!(BASE64_STANDARD.encode(canonicalized_body));
        let bundle = serde_json::to_vec(&bundle).unwrap();
        let http = signed_registry_http(&bundle, 200);
        let mut lock_file = LockFile::default();

        let error = resolve_provider_with_http(
            dir.path(),
            "carina-rs/aws",
            "0.5.0",
            "aws",
            &mut lock_file,
            &http,
        )
        .unwrap_err();

        assert!(error.contains("signature verification"), "{error}");
        assert!(error.contains("no override"), "{error}");
        assert!(
            error.contains("registry provider 'aws' (carina-rs/aws@0.5.0)"),
            "{error}"
        );
        assert!(!cache_path_wasm(dir.path(), "carina-rs/aws", "0.5.0").exists());
    }

    #[test]
    fn registry_source_rejects_unknown_signature_type() {
        let dir = tempfile::tempdir().unwrap();
        let shasum = sha256_bytes(SIGNED_FIXTURE_ARTIFACT);
        let http = signed_registry_http(SIGNED_FIXTURE_BUNDLE, 200).json(
            "https://registry.carina-rs.dev/v1/providers/carina-rs/aws/0.5.0/download",
            &format!(
                r#"{{"download_url":"https://downloads.example.test/aws.wasm","shasum":"{shasum}","signature":{{"type":"something-else","certificate_identity":"{SIGNED_FIXTURE_IDENTITY}","certificate_oidc_issuer":"{SIGNED_FIXTURE_ISSUER}","bundle_url":"{SIGNED_FIXTURE_BUNDLE_URL}"}}}}"#
            ),
        );
        let mut lock_file = LockFile::default();

        let error = resolve_provider_with_http(
            dir.path(),
            "carina-rs/aws",
            "0.5.0",
            "aws",
            &mut lock_file,
            &http,
        )
        .unwrap_err();

        assert!(error.contains("something-else"), "{error}");
        assert!(!http.was_requested("aws.wasm"));
    }

    #[test]
    fn registry_ratchet_unsupported_signature_type_records_transparency_log_presence() {
        let dir = tempfile::tempdir().unwrap();
        let shasum = sha256_bytes(SIGNED_FIXTURE_ARTIFACT);
        let http = signed_registry_http(SIGNED_FIXTURE_BUNDLE, 200).json(
            REGISTRY_DOWNLOAD_URL,
            &format!(
                r#"{{"download_url":"https://downloads.example.test/aws.wasm","shasum":"{shasum}","signature":{{"type":"something-else","certificate_identity":"{SIGNED_FIXTURE_IDENTITY}","certificate_oidc_issuer":"{SIGNED_FIXTURE_ISSUER}","bundle_url":"{SIGNED_FIXTURE_BUNDLE_URL}"}},"transparency_log":{{"observed":true}}}}"#
            ),
        );
        let mut lock_file = LockFile::default();

        let error = resolve_provider_with_http(
            dir.path(),
            "carina-rs/aws",
            "0.5.0",
            "aws",
            &mut lock_file,
            &http,
        )
        .unwrap_err();

        assert!(error.contains("something-else"), "{error}");
        let lock_contents = saved_lock_contents(dir.path());
        assert!(
            lock_contents.contains("transparency_log_present = true"),
            "the accepted presence promotion must be durable before signature-type rejection: {lock_contents}"
        );
        assert!(!lock_contents.contains(SIGNED_FIXTURE_IDENTITY));
        assert!(!http.was_requested("aws.wasm"));
    }

    #[test]
    fn registry_source_rejects_signature_bundle_http_error() {
        let dir = tempfile::tempdir().unwrap();
        let initial_http = signed_registry_http(SIGNED_FIXTURE_BUNDLE, 200);
        let mut lock_file = LockFile::default();
        resolve_provider_with_http(
            dir.path(),
            "carina-rs/aws",
            "0.5.0",
            "aws",
            &mut lock_file,
            &initial_http,
        )
        .unwrap();
        let http = signed_registry_http(b"not found", 404);

        let error = resolve_provider_with_http(
            dir.path(),
            "carina-rs/aws",
            "0.5.0",
            "aws",
            &mut lock_file,
            &http,
        )
        .unwrap_err();

        assert!(error.contains("404"), "{error}");
        assert!(
            error.contains("cannot fetch the signature bundle"),
            "{error}"
        );
        assert!(
            error.contains("signature verification cannot proceed"),
            "{error}"
        );
        assert!(error.contains("no override"), "{error}");
        assert!(
            error.contains("registry provider 'aws' (carina-rs/aws@0.5.0)"),
            "{error}"
        );
        assert!(
            !error.contains("Sigstore signature verification failed"),
            "{error}"
        );
        let wasm_path = cache_path_wasm(dir.path(), "carina-rs/aws", "0.5.0");
        assert_eq!(fs::read(wasm_path).unwrap(), SIGNED_FIXTURE_ARTIFACT);
        assert_eq!(
            http.request_count("https://downloads.example.test/aws.wasm"),
            0
        );
    }

    #[test]
    fn registry_source_rejects_signature_bundle_transport_error() {
        let dir = tempfile::tempdir().unwrap();
        let shasum = sha256_bytes(SIGNED_FIXTURE_ARTIFACT);
        let unavailable_bundle_url = "https://downloads.example.test/unavailable.sigstore.json";
        let http = signed_registry_http(SIGNED_FIXTURE_BUNDLE, 200).json(
            "https://registry.carina-rs.dev/v1/providers/carina-rs/aws/0.5.0/download",
            &format!(
                r#"{{"download_url":"https://downloads.example.test/aws.wasm","shasum":"{shasum}","signature":{{"type":"sigstore-bundle","certificate_identity":"{SIGNED_FIXTURE_IDENTITY}","certificate_oidc_issuer":"{SIGNED_FIXTURE_ISSUER}","bundle_url":"{unavailable_bundle_url}"}}}}"#
            ),
        );
        let mut lock_file = LockFile::default();

        let error = resolve_provider_with_http(
            dir.path(),
            "carina-rs/aws",
            "0.5.0",
            "aws",
            &mut lock_file,
            &http,
        )
        .unwrap_err();

        assert!(
            error.contains("cannot fetch the signature bundle"),
            "{error}"
        );
        assert!(error.contains("unexpected test URL"), "{error}");
        assert!(
            error.contains("signature verification cannot proceed"),
            "{error}"
        );
        assert!(error.contains("no override"), "{error}");
        assert!(
            error.contains("registry provider 'aws' (carina-rs/aws@0.5.0)"),
            "{error}"
        );
        assert!(
            !error.contains("Sigstore signature verification failed"),
            "{error}"
        );
        assert_eq!(
            fs::read(cache_path_wasm(dir.path(), "carina-rs/aws", "0.5.0")).unwrap(),
            SIGNED_FIXTURE_ARTIFACT
        );
    }

    #[test]
    fn registry_source_rejects_malformed_signature_block() {
        let dir = tempfile::tempdir().unwrap();
        let shasum = sha256_bytes(SIGNED_FIXTURE_ARTIFACT);
        let http = signed_registry_http(SIGNED_FIXTURE_BUNDLE, 200).json(
            "https://registry.carina-rs.dev/v1/providers/carina-rs/aws/0.5.0/download",
            &format!(
                r#"{{"download_url":"https://downloads.example.test/aws.wasm","shasum":"{shasum}","signature":{{"type":"sigstore-bundle"}}}}"#
            ),
        );
        let mut lock_file = LockFile::default();

        let error = resolve_provider_with_http(
            dir.path(),
            "carina-rs/aws",
            "0.5.0",
            "aws",
            &mut lock_file,
            &http,
        )
        .unwrap_err();

        assert!(error.contains("signature"), "{error}");
        assert!(error.contains("no override"), "{error}");
        assert!(!http.was_requested("aws.wasm"));
    }

    #[test]
    fn registry_source_does_not_treat_null_signature_as_absent() {
        let dir = tempfile::tempdir().unwrap();
        let shasum = sha256_bytes(SIGNED_FIXTURE_ARTIFACT);
        let http = signed_registry_http(SIGNED_FIXTURE_BUNDLE, 200).json(
            "https://registry.carina-rs.dev/v1/providers/carina-rs/aws/0.5.0/download",
            &format!(
                r#"{{"download_url":"https://downloads.example.test/aws.wasm","shasum":"{shasum}","signature":null}}"#
            ),
        );
        let mut lock_file = LockFile::default();

        let error = resolve_provider_with_http(
            dir.path(),
            "carina-rs/aws",
            "0.5.0",
            "aws",
            &mut lock_file,
            &http,
        )
        .unwrap_err();

        assert!(
            error.starts_with("Failed to parse registry JSON from"),
            "{error}"
        );
        assert!(error.contains("malformed registry signature"), "{error}");
        assert!(error.contains("no override"), "{error}");
        assert!(!http.was_requested("aws.wasm"));
    }

    #[test]
    fn registry_source_resolves_sections_1_2_3_and_verifies_registry_shasum() {
        let dir = tempfile::tempdir().unwrap();
        let body = b"registry wasm bytes";
        let shasum = sha256_bytes(body);
        let http = registry_http(body, &shasum);
        let mut lock_file = LockFile::default();

        let path = resolve_provider_with_http(
            dir.path(),
            "carina-rs/aws",
            "0.5.0",
            "aws",
            &mut lock_file,
            &http,
        )
        .unwrap();

        assert_eq!(fs::read(&path).unwrap(), body);
        assert!(http.was_requested("/.well-known/carina.json"));
        assert!(http.was_requested("/v1/providers/carina-rs/aws/versions"));
        assert!(http.was_requested("/v1/providers/carina-rs/aws/0.5.0/download"));

        let entry = lock_file.find_by_source("carina-rs/aws").unwrap();
        assert_eq!(entry.sha256, shasum);
        let registry = entry
            .registry
            .as_ref()
            .expect("registry pin must be recorded");
        assert_eq!(registry.resolved_hostname(), "registry.carina-rs.dev");
        let host_pin = lock_file
            .registry_host
            .get(registry.resolved_hostname())
            .and_then(RegistryHostLock::pin)
            .expect("registry host pin must be recorded");
        assert_eq!(
            host_pin.api_base_url(),
            "https://registry.carina-rs.dev/v1/providers/"
        );
        assert_eq!(registry.sequence, RegistrySequence::Present(7));
        assert_eq!(registry.signature, RegistrySignatureProtection::NotRequired);
    }

    #[test]
    fn registry_discovery_pin_ignores_document_reserialization() {
        let dir = tempfile::tempdir().unwrap();
        let mut lock_file = LockFile::default();
        let api_base_url = "https://registry.carina-rs.dev/v1/providers/";

        resolve_provider_with_http(
            dir.path(),
            "carina-rs/aws",
            "0.5.0",
            "aws",
            &mut lock_file,
            &registry_http_with_discovery(r#"{"providers.v1":"/v1/providers/"}"#, api_base_url),
        )
        .unwrap();

        resolve_provider_with_http(
            dir.path(),
            "carina-rs/aws",
            "0.5.0",
            "aws",
            &mut lock_file,
            &registry_http_with_discovery(
                "{\n  \"providers.v1\": \"/v1/providers/\"\n}\n",
                api_base_url,
            ),
        )
        .expect("different JSON bytes with the same consumed value must retain the pin");
    }

    #[test]
    fn registry_discovery_pin_ignores_unconsumed_fields() {
        let dir = tempfile::tempdir().unwrap();
        let mut lock_file = LockFile::default();
        let api_base_url = "https://registry.carina-rs.dev/v1/providers/";

        resolve_provider_with_http(
            dir.path(),
            "carina-rs/aws",
            "0.5.0",
            "aws",
            &mut lock_file,
            &registry_http_with_discovery(r#"{"providers.v1":"/v1/providers/"}"#, api_base_url),
        )
        .unwrap();

        resolve_provider_with_http(
            dir.path(),
            "carina-rs/aws",
            "0.5.0",
            "aws",
            &mut lock_file,
            &registry_http_with_discovery(
                r#"{"modules.v1":"/v1/modules/","providers.v1":"/v1/providers/"}"#,
                api_base_url,
            ),
        )
        .expect("a discovery field this client does not consume must not trip the pin");
    }

    #[test]
    fn registry_discovery_pin_rejects_relocated_providers_api_with_repin_remediation() {
        let dir = tempfile::tempdir().unwrap();
        let mut lock_file = LockFile::default();

        resolve_provider_with_http(
            dir.path(),
            "carina-rs/aws",
            "0.5.0",
            "aws",
            &mut lock_file,
            &registry_http_with_discovery(
                r#"{"providers.v1":"/v1/providers/"}"#,
                "https://registry.carina-rs.dev/v1/providers/",
            ),
        )
        .unwrap();

        let relocated_http = registry_http_with_discovery(
            r#"{"providers.v1":"/v2/providers/"}"#,
            "https://registry.carina-rs.dev/v2/providers/",
        );
        let error = resolve_provider_with_http(
            dir.path(),
            "carina-rs/aws",
            "0.5.0",
            "aws",
            &mut lock_file,
            &relocated_http,
        )
        .unwrap_err();

        assert!(
            error.contains("pinned discovery values mismatch"),
            "{error}"
        );
        assert!(
            error.contains("carina providers repin-discovery registry.carina-rs.dev"),
            "{error}"
        );
        assert!(
            !relocated_http.was_requested("/v2/providers/carina-rs/aws"),
            "a changed API base was used before its pin was verified"
        );
    }

    #[test]
    fn ureq_transport_selects_redirect_policy_by_request_type() {
        let discovery = UreqRegistryHttp::agent(RegistryHttpRequest::Discovery(
            "https://registry.example.test/.well-known/carina.json",
        ));
        let resource = UreqRegistryHttp::agent(RegistryHttpRequest::Resource(
            "https://registry.example.test/v1/providers/",
        ));

        assert_eq!(discovery.config().max_redirects(), 0);
        assert_eq!(resource.config().max_redirects(), 10);
    }

    #[test]
    fn registry_discovery_redirect_is_fetch_failure_without_repin_remediation() {
        const DISCOVERY_URL: &str = "https://registry.carina-rs.dev/.well-known/carina.json";
        const REDIRECT_TARGET: &str =
            "https://redirect.example.test/relocated-caring-discovery.json";

        let dir = tempfile::tempdir().unwrap();
        let mut lock_file = LockFile::default();
        let api_base_url = "https://registry.carina-rs.dev/v1/providers/";
        resolve_provider_with_http(
            dir.path(),
            "carina-rs/aws",
            "0.5.0",
            "aws",
            &mut lock_file,
            &registry_http_with_discovery(r#"{"providers.v1":"/v1/providers/"}"#, api_base_url),
        )
        .unwrap();

        let redirected_http = registry_http_with_discovery(
            "{\n  \"providers.v1\": \"/v1/providers/\"\n}\n",
            api_base_url,
        )
        .redirect(DISCOVERY_URL, 302, REDIRECT_TARGET)
        .json(
            REDIRECT_TARGET,
            "{\n  \"providers.v1\": \"/v1/providers/\"\n}\n",
        );
        let error = resolve_provider_with_http(
            dir.path(),
            "carina-rs/aws",
            "0.5.0",
            "aws",
            &mut lock_file,
            &redirected_http,
        )
        .unwrap_err();

        assert!(error.contains("redirect status 302"), "{error}");
        assert!(!error.contains("pin mismatch"), "{error}");
        assert!(
            !error.contains("carina providers repin-discovery"),
            "{error}"
        );
        assert!(
            !redirected_http.was_requested(REDIRECT_TARGET),
            "the discovery client followed the redirect to {REDIRECT_TARGET}"
        );
    }

    #[test]
    fn registry_discovery_pin_compares_resolved_values_by_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let mut lock_file = LockFile::default();

        resolve_provider_with_http(
            dir.path(),
            "carina-rs/aws",
            "0.5.0",
            "aws",
            &mut lock_file,
            &registry_http_with_discovery(
                r#"{"providers.v1":"/v1/providers/"}"#,
                "https://registry.carina-rs.dev/v1/providers/",
            ),
        )
        .unwrap();

        let error = resolve_provider_with_http(
            dir.path(),
            "carina-rs/aws",
            "0.5.0",
            "aws",
            &mut lock_file,
            &registry_http_with_discovery(
                r#"{"providers.v1":"/v1/%70roviders/"}"#,
                "https://registry.carina-rs.dev/v1/%70roviders/",
            ),
        )
        .unwrap_err();

        assert!(
            error.contains("pinned discovery values mismatch"),
            "{error}"
        );
        assert!(
            error.contains("pinned providers.v1 was https://registry.carina-rs.dev/v1/providers/"),
            "{error}"
        );
        assert!(
            error.contains(
                "resolved providers.v1 is https://registry.carina-rs.dev/v1/%70roviders/"
            ),
            "{error}"
        );
    }

    #[test]
    fn registry_revision_resolves_to_version_download_and_records_revision_lock() {
        let dir = tempfile::tempdir().unwrap();
        let body = b"registry revision wasm bytes";
        let shasum = sha256_bytes(body);
        let http = FakeRegistryHttp::default()
            .json(
                "https://registry.carina-rs.dev/.well-known/carina.json",
                r#"{"providers.v1":"/v1/providers/"}"#,
            )
            .json(
                "https://registry.carina-rs.dev/v1/providers/carina-rs/aws/versions",
                r#"{"sequence":7,"valid_until":"2999-01-01T00:00:00Z","versions":[{"version":"0.0.0-main.1.aaa","protocols":["1"]},{"version":"0.0.0-main.10.bbb","protocols":["1"]},{"version":"0.5.0","protocols":["1"]},{"version":"0.0.0-dev.2.ccc","protocols":["1"]}]}"#,
            )
            .json(
                "https://registry.carina-rs.dev/v1/providers/carina-rs/aws/0.0.0-main.10.bbb/download",
                &format!(
                    r#"{{
                        "protocols":["1"],
                        "filename":"carina-provider-aws-v0.0.0-main.10.bbb.wasm",
                        "download_url":"https://downloads.example.test/aws-main.wasm",
                        "shasum":"{shasum}",
                        "shasum_authored_by":"registry"
                    }}"#
                ),
            )
            .bytes("https://downloads.example.test/aws-main.wasm", body)
            .downloadable_bytes("https://downloads.example.test/aws-main.wasm", body);
        let config = ProviderConfig {
            name: "aws".into(),
            source: Some("carina-rs/aws".into()),
            version: None,
            revision: Some("main".into()),
            attributes: IndexMap::new(),
            default_tags: IndexMap::new(),
            unresolved_attributes: IndexMap::new(),
            binding: None,
            is_default: true,
        };

        let path = resolve_single_config_with_http(dir.path(), &config, &http).unwrap();

        assert_eq!(fs::read(&path).unwrap(), body);
        assert!(http.was_requested("/.well-known/carina.json"));
        assert!(http.was_requested("/v1/providers/carina-rs/aws/versions"));
        assert!(http.was_requested("/v1/providers/carina-rs/aws/0.0.0-main.10.bbb/download"));
        assert!(!http.was_requested("/revisions/main/download"));

        let lock = LockFile::load(&dir.path().join("carina-providers.lock"))
            .unwrap()
            .unwrap();
        let entry = lock.find_by_source("carina-rs/aws").unwrap();
        assert_eq!(entry.sha256, shasum);
        assert!(entry.registry.is_some());
        assert_eq!(
            entry.kind,
            LockEntryKind::RegistryRevision {
                revision: "main".into(),
                version: "0.0.0-main.10.bbb".into(),
            }
        );
    }

    #[test]
    fn registry_revision_upgrade_re_resolves_newer_branch_prerelease() {
        let dir = tempfile::tempdir().unwrap();
        let old_body = b"old registry revision wasm bytes";
        let old_shasum = sha256_bytes(old_body);
        let new_body = b"new registry revision wasm bytes";
        let new_shasum = sha256_bytes(new_body);
        let mut lock = LockFile::default();
        lock.upsert_registry(
            LockEntry {
                name: "aws".into(),
                source: "carina-rs/aws".into(),
                kind: LockEntryKind::RegistryRevision {
                    revision: "main".into(),
                    version: "0.0.0-main.1.aaa".into(),
                },
                sha256: old_shasum,
                registry: Some(RegistryLock {
                    resolved_hostname: "registry.carina-rs.dev".into(),
                    sequence: RegistrySequence::Present(7),
                    sequence_anchor: RegistrySequenceAnchor::Established(7),
                    valid_until_present: true,
                    yanked_versions: YankedRegistryVersions::default(),
                    signature: RegistrySignatureProtection::NotRequired,
                    transparency_log_present: false,
                }),
            },
            registry_host_lock("https://registry.carina-rs.dev/v1/providers/"),
        )
        .unwrap();
        lock.save(&dir.path().join("carina-providers.lock"))
            .unwrap();
        let http = FakeRegistryHttp::default()
            .json(
                "https://registry.carina-rs.dev/.well-known/carina.json",
                r#"{"providers.v1":"/v1/providers/"}"#,
            )
            .json(
                "https://registry.carina-rs.dev/v1/providers/carina-rs/aws/versions",
                r#"{"sequence":8,"valid_until":"2999-01-01T00:00:00Z","versions":[{"version":"0.0.0-main.1.aaa","protocols":["1"]},{"version":"0.0.0-main.10.bbb","protocols":["1"]}]}"#,
            )
            .json(
                "https://registry.carina-rs.dev/v1/providers/carina-rs/aws/0.0.0-main.10.bbb/download",
                &format!(
                    r#"{{"protocols":["1"],"filename":"aws.wasm","download_url":"https://downloads.example.test/aws-main-10.wasm","shasum":"{new_shasum}","shasum_authored_by":"registry"}}"#
                ),
            )
            .bytes("https://downloads.example.test/aws-main-10.wasm", new_body)
            .downloadable_bytes("https://downloads.example.test/aws-main-10.wasm", new_body);
        let config = ProviderConfig {
            name: "aws".into(),
            source: Some("carina-rs/aws".into()),
            version: None,
            revision: Some("main".into()),
            attributes: IndexMap::new(),
            default_tags: IndexMap::new(),
            unresolved_attributes: IndexMap::new(),
            binding: None,
            is_default: true,
        };

        let resolved =
            resolve_all_with_http(dir.path(), &[config], LockMode::Upgrade, &http).unwrap();

        assert_eq!(fs::read(resolved.get("aws").unwrap()).unwrap(), new_body);
        assert!(http.was_requested("/v1/providers/carina-rs/aws/versions"));
        assert!(http.was_requested("/v1/providers/carina-rs/aws/0.0.0-main.10.bbb/download"));
        let lock = LockFile::load(&dir.path().join("carina-providers.lock"))
            .unwrap()
            .unwrap();
        assert_eq!(
            lock.find_by_source("carina-rs/aws").unwrap().kind,
            LockEntryKind::RegistryRevision {
                revision: "main".into(),
                version: "0.0.0-main.10.bbb".into(),
            }
        );
    }

    #[test]
    fn registry_revision_reinit_reuses_locked_version_without_reselecting() {
        let dir = tempfile::tempdir().unwrap();
        let body = b"locked registry revision wasm bytes";
        let shasum = sha256_bytes(body);
        let mut lock = LockFile::default();
        lock.upsert_registry(
            LockEntry {
                name: "aws".into(),
                source: "carina-rs/aws".into(),
                kind: LockEntryKind::RegistryRevision {
                    revision: "main".into(),
                    version: "0.0.0-main.1.aaa".into(),
                },
                sha256: shasum.clone(),
                registry: Some(RegistryLock {
                    resolved_hostname: "registry.carina-rs.dev".into(),
                    sequence: RegistrySequence::Present(7),
                    sequence_anchor: RegistrySequenceAnchor::Established(7),
                    valid_until_present: true,
                    yanked_versions: YankedRegistryVersions::default(),
                    signature: RegistrySignatureProtection::NotRequired,
                    transparency_log_present: false,
                }),
            },
            registry_host_lock("https://registry.carina-rs.dev/v1/providers/"),
        )
        .unwrap();
        lock.save(&dir.path().join("carina-providers.lock"))
            .unwrap();
        let http = FakeRegistryHttp::default()
            .json(
                "https://registry.carina-rs.dev/.well-known/carina.json",
                r#"{"providers.v1":"/v1/providers/"}"#,
            )
            .json(
                "https://registry.carina-rs.dev/v1/providers/carina-rs/aws/versions",
                r#"{"sequence":8,"valid_until":"2999-01-01T00:00:00Z","versions":[{"version":"0.0.0-main.1.aaa","protocols":["1"]},{"version":"0.0.0-main.10.bbb","protocols":["1"]}]}"#,
            )
            .json(
                "https://registry.carina-rs.dev/v1/providers/carina-rs/aws/0.0.0-main.1.aaa/download",
                &format!(
                    r#"{{"protocols":["1"],"filename":"aws.wasm","download_url":"https://downloads.example.test/aws-main-1.wasm","shasum":"{shasum}","shasum_authored_by":"registry"}}"#
                ),
            )
            .bytes("https://downloads.example.test/aws-main-1.wasm", body)
            .downloadable_bytes("https://downloads.example.test/aws-main-1.wasm", body);
        let config = ProviderConfig {
            name: "aws".into(),
            source: Some("carina-rs/aws".into()),
            version: None,
            revision: Some("main".into()),
            attributes: IndexMap::new(),
            default_tags: IndexMap::new(),
            unresolved_attributes: IndexMap::new(),
            binding: None,
            is_default: true,
        };

        let resolved =
            resolve_all_with_http(dir.path(), &[config], LockMode::Normal, &http).unwrap();

        assert_eq!(fs::read(resolved.get("aws").unwrap()).unwrap(), body);
        assert!(http.was_requested("/v1/providers/carina-rs/aws/0.0.0-main.1.aaa/download"));
        assert!(!http.was_requested("/v1/providers/carina-rs/aws/0.0.0-main.10.bbb/download"));
    }

    #[test]
    fn github_direct_revision_does_not_use_registry_resolution() {
        let config = ProviderConfig {
            name: "aws".into(),
            source: None,
            version: None,
            revision: Some("main".into()),
            attributes: IndexMap::new(),
            default_tags: IndexMap::new(),
            unresolved_attributes: IndexMap::new(),
            binding: None,
            is_default: true,
        };

        assert_eq!(
            registry_revision("github.com/carina-rs/carina-provider-aws", &config),
            Ok(None),
            "github-direct revisions must not route through registry resolution"
        );
        assert_eq!(
            registry_revision("carina-rs/aws", &config),
            Ok(Some("main")),
            "registry source revisions must route through registry resolution"
        );
    }

    #[test]
    fn registry_source_rejects_wasm_when_registry_shasum_mismatches() {
        let dir = tempfile::tempdir().unwrap();
        let http = registry_http(
            b"tampered wasm bytes",
            &sha256_bytes(b"expected wasm bytes"),
        );
        let mut lock_file = LockFile::default();

        let err = resolve_provider_with_http(
            dir.path(),
            "carina-rs/aws",
            "0.5.0",
            "aws",
            &mut lock_file,
            &http,
        )
        .unwrap_err();

        assert!(
            err.to_lowercase().contains("sha256"),
            "mismatch must fail closed before lock/cache use: {err}"
        );
        assert!(
            lock_file.find_by_source("carina-rs/aws").is_none(),
            "mismatched bytes must not be pinned"
        );
    }

    #[test]
    fn registry_source_removes_cached_wasm_when_registry_shasum_mismatches() {
        let dir = tempfile::tempdir().unwrap();
        let body = b"expected wasm bytes";
        let shasum = sha256_bytes(body);
        let http = registry_http(body, &shasum);
        let mut lock_file = LockFile::default();
        let cached_path = cache_path_wasm(dir.path(), "carina-rs/aws", "0.5.0");
        fs::create_dir_all(cached_path.parent().unwrap()).unwrap();
        fs::write(&cached_path, b"stale cached wasm bytes").unwrap();

        let err = resolve_provider_with_http(
            dir.path(),
            "carina-rs/aws",
            "0.5.0",
            "aws",
            &mut lock_file,
            &http,
        )
        .unwrap_err();

        assert!(err.to_lowercase().contains("sha256"), "{err}");
        assert!(
            !cached_path.exists(),
            "bad cached WASM must be removed so the next run can re-download"
        );
        assert!(
            lock_file.find_by_source("carina-rs/aws").is_none(),
            "mismatched cached bytes must not be pinned"
        );
    }

    #[test]
    fn registry_source_rejects_lower_sequence_than_lock_pin() {
        let dir = tempfile::tempdir().unwrap();
        let body = b"registry wasm bytes";
        let shasum = sha256_bytes(body);
        let http = FakeRegistryHttp::default()
            .json(
                "https://registry.carina-rs.dev/.well-known/carina.json",
                r#"{"providers.v1":"/v1/providers/"}"#,
            )
            .json(
                "https://registry.carina-rs.dev/v1/providers/carina-rs/aws/versions",
                r#"{"sequence":6,"valid_until":"2999-01-01T00:00:00Z","versions":[{"version":"0.5.0","protocols":["1"]}]}"#,
            )
            .json(
                "https://registry.carina-rs.dev/v1/providers/carina-rs/aws/0.5.0/download",
                &format!(
                    r#"{{"protocols":["1"],"filename":"aws.wasm","download_url":"https://downloads.example.test/aws.wasm","shasum":"{shasum}","shasum_authored_by":"registry"}}"#
                ),
            )
            .bytes("https://downloads.example.test/aws.wasm", body);
        let mut lock_file = LockFile::default();
        lock_file
            .upsert_registry(
                LockEntry {
                    name: "aws".into(),
                    source: "carina-rs/aws".into(),
                    kind: LockEntryKind::Version {
                        version: "0.5.0".into(),
                        constraint: None,
                    },
                    sha256: shasum,
                    registry: Some(RegistryLock {
                        resolved_hostname: "registry.carina-rs.dev".into(),
                        sequence: RegistrySequence::Present(7),
                        sequence_anchor: RegistrySequenceAnchor::Established(7),
                        valid_until_present: true,
                        yanked_versions: YankedRegistryVersions::default(),
                        signature: RegistrySignatureProtection::NotRequired,
                        transparency_log_present: false,
                    }),
                },
                registry_host_lock("https://registry.carina-rs.dev/v1/providers/"),
            )
            .unwrap();

        let err = resolve_provider_with_http(
            dir.path(),
            "carina-rs/aws",
            "0.5.0",
            "aws",
            &mut lock_file,
            &http,
        )
        .unwrap_err();
        assert!(err.contains("sequence"), "{err}");
        assert!(
            err.contains("carina providers re-bootstrap carina-rs/aws"),
            "{err}"
        );
    }

    #[test]
    fn registry_source_rejects_missing_previously_present_sequence() {
        let dir = tempfile::tempdir().unwrap();
        let body = b"registry wasm bytes";
        let shasum = sha256_bytes(body);
        let http = registry_http(body, &shasum).json(
            "https://registry.carina-rs.dev/v1/providers/carina-rs/aws/versions",
            r#"{"valid_until":"2999-01-01T00:00:00Z","versions":[{"version":"0.5.0","protocols":["1"]}]}"#,
        );
        let mut lock_file = LockFile::default();
        lock_file
            .upsert_registry(
                LockEntry {
                    name: "aws".into(),
                    source: "carina-rs/aws".into(),
                    kind: LockEntryKind::Version {
                        version: "0.5.0".into(),
                        constraint: None,
                    },
                    sha256: shasum,
                    registry: Some(RegistryLock {
                        resolved_hostname: "registry.carina-rs.dev".into(),
                        sequence: RegistrySequence::Present(7),
                        sequence_anchor: RegistrySequenceAnchor::Established(7),
                        valid_until_present: false,
                        yanked_versions: YankedRegistryVersions::default(),
                        signature: RegistrySignatureProtection::NotRequired,
                        transparency_log_present: false,
                    }),
                },
                registry_host_lock("https://registry.carina-rs.dev/v1/providers/"),
            )
            .unwrap();

        let err = resolve_provider_with_http(
            dir.path(),
            "carina-rs/aws",
            "0.5.0",
            "aws",
            &mut lock_file,
            &http,
        )
        .unwrap_err();
        assert!(err.contains("sequence"), "{err}");
    }

    #[test]
    fn registry_source_rejects_missing_previously_present_valid_until() {
        let dir = tempfile::tempdir().unwrap();
        let body = b"registry wasm bytes";
        let shasum = sha256_bytes(body);
        let http = registry_http(body, &shasum).json(
            "https://registry.carina-rs.dev/v1/providers/carina-rs/aws/versions",
            r#"{"sequence":7,"versions":[{"version":"0.5.0","protocols":["1"]}]}"#,
        );
        let mut lock_file = LockFile::default();
        lock_file
            .upsert_registry(
                LockEntry {
                    name: "aws".into(),
                    source: "carina-rs/aws".into(),
                    kind: LockEntryKind::Version {
                        version: "0.5.0".into(),
                        constraint: None,
                    },
                    sha256: shasum,
                    registry: Some(RegistryLock {
                        resolved_hostname: "registry.carina-rs.dev".into(),
                        sequence: RegistrySequence::Present(7),
                        sequence_anchor: RegistrySequenceAnchor::Established(7),
                        valid_until_present: true,
                        yanked_versions: YankedRegistryVersions::default(),
                        signature: RegistrySignatureProtection::NotRequired,
                        transparency_log_present: false,
                    }),
                },
                registry_host_lock("https://registry.carina-rs.dev/v1/providers/"),
            )
            .unwrap();

        let err = resolve_provider_with_http(
            dir.path(),
            "carina-rs/aws",
            "0.5.0",
            "aws",
            &mut lock_file,
            &http,
        )
        .unwrap_err();
        assert!(err.contains("valid_until"), "{err}");
    }

    #[test]
    fn registry_source_rejects_absurd_sequence_fast_forward() {
        let dir = tempfile::tempdir().unwrap();
        let body = b"registry wasm bytes";
        let shasum = sha256_bytes(body);
        let http = registry_http(body, &shasum).json(
            "https://registry.carina-rs.dev/v1/providers/carina-rs/aws/versions",
            r#"{"sequence":1000000008,"valid_until":"2999-01-01T00:00:00Z","versions":[{"version":"0.5.0","protocols":["1"]}]}"#,
        );
        let mut lock_file = LockFile::default();
        lock_file
            .upsert_registry(
                LockEntry {
                    name: "aws".into(),
                    source: "carina-rs/aws".into(),
                    kind: LockEntryKind::Version {
                        version: "0.5.0".into(),
                        constraint: None,
                    },
                    sha256: shasum,
                    registry: Some(RegistryLock {
                        resolved_hostname: "registry.carina-rs.dev".into(),
                        sequence: RegistrySequence::Present(7),
                        sequence_anchor: RegistrySequenceAnchor::Established(7),
                        valid_until_present: true,
                        yanked_versions: YankedRegistryVersions::default(),
                        signature: RegistrySignatureProtection::NotRequired,
                        transparency_log_present: false,
                    }),
                },
                registry_host_lock("https://registry.carina-rs.dev/v1/providers/"),
            )
            .unwrap();

        let err = resolve_provider_with_http(
            dir.path(),
            "carina-rs/aws",
            "0.5.0",
            "aws",
            &mut lock_file,
            &http,
        )
        .unwrap_err();
        assert!(err.contains("sequence fast-forward"), "{err}");
        assert!(
            err.contains("carina providers re-bootstrap carina-rs/aws"),
            "{err}"
        );
    }

    #[test]
    fn registry_source_rejects_missing_previously_present_signature() {
        let dir = tempfile::tempdir().unwrap();
        let body = b"registry wasm bytes";
        let shasum = sha256_bytes(body);
        let http = registry_http(body, &shasum).json(
            "https://registry.carina-rs.dev/v1/providers/carina-rs/aws/0.5.0/download",
            &format!(
                r#"{{"protocols":["1"],"filename":"aws.wasm","download_url":"https://downloads.example.test/aws.wasm","shasum":"{shasum}","shasum_authored_by":"registry"}}"#
            ),
        );
        let mut lock_file = LockFile::default();
        lock_file
            .upsert_registry(
                LockEntry {
                    name: "aws".into(),
                    source: "carina-rs/aws".into(),
                    kind: LockEntryKind::Version {
                        version: "0.5.0".into(),
                        constraint: None,
                    },
                    sha256: shasum,
                    registry: Some(RegistryLock {
                        resolved_hostname: "registry.carina-rs.dev".into(),
                        sequence: RegistrySequence::Present(7),
                        sequence_anchor: RegistrySequenceAnchor::Established(7),
                        valid_until_present: true,
                        yanked_versions: YankedRegistryVersions::default(),
                        signature: signed_fixture_pin(),
                        transparency_log_present: false,
                    }),
                },
                registry_host_lock("https://registry.carina-rs.dev/v1/providers/"),
            )
            .unwrap();

        let err = resolve_provider_with_http(
            dir.path(),
            "carina-rs/aws",
            "0.5.0",
            "aws",
            &mut lock_file,
            &http,
        )
        .unwrap_err();
        assert!(
            err.contains(
                "the resolved version of carina-rs/aws has no registry signature, but carina-providers.lock records signatures as required for this provider"
            ),
            "{err}"
        );
        assert!(
            err.contains("downgrades from signed to unsigned versions are refused"),
            "{err}"
        );
        assert!(
            err.contains("carina providers repin-identity carina-rs/aws"),
            "{err}"
        );
    }

    #[test]
    fn registry_source_rejects_signature_downgrade_across_version_change() {
        let dir = tempfile::tempdir().unwrap();
        let old_body = b"old registry wasm bytes";
        let old_shasum = sha256_bytes(old_body);
        let new_body = b"new registry wasm bytes";
        let new_shasum = sha256_bytes(new_body);
        let http = FakeRegistryHttp::default()
            .json(
                "https://registry.carina-rs.dev/.well-known/carina.json",
                r#"{"providers.v1":"/v1/providers/"}"#,
            )
            .json(
                "https://registry.carina-rs.dev/v1/providers/carina-rs/aws/versions",
                r#"{"sequence":7,"valid_until":"2999-01-01T00:00:00Z","versions":[{"version":"0.5.0","protocols":["1"]},{"version":"0.6.0","protocols":["1"]}]}"#,
            )
            .json(
                "https://registry.carina-rs.dev/v1/providers/carina-rs/aws/0.6.0/download",
                &format!(
                    r#"{{"protocols":["1"],"filename":"aws.wasm","download_url":"https://downloads.example.test/aws-0.6.0.wasm","shasum":"{new_shasum}","shasum_authored_by":"registry"}}"#
                ),
            )
            .bytes("https://downloads.example.test/aws-0.6.0.wasm", new_body)
            .downloadable_bytes("https://downloads.example.test/aws-0.6.0.wasm", new_body);
        let mut lock_file = LockFile::default();
        lock_file
            .upsert_registry(
                LockEntry {
                    name: "aws".into(),
                    source: "carina-rs/aws".into(),
                    kind: LockEntryKind::Version {
                        version: "0.5.0".into(),
                        constraint: None,
                    },
                    sha256: old_shasum,
                    registry: Some(RegistryLock {
                        resolved_hostname: "registry.carina-rs.dev".into(),
                        sequence: RegistrySequence::Present(7),
                        sequence_anchor: RegistrySequenceAnchor::Established(7),
                        valid_until_present: true,
                        yanked_versions: YankedRegistryVersions::default(),
                        signature: signed_fixture_pin(),
                        transparency_log_present: false,
                    }),
                },
                registry_host_lock("https://registry.carina-rs.dev/v1/providers/"),
            )
            .unwrap();

        let err = resolve_provider_with_http(
            dir.path(),
            "carina-rs/aws",
            "0.6.0",
            "aws",
            &mut lock_file,
            &http,
        )
        .unwrap_err();
        assert!(err.contains("signature"), "{err}");
    }

    #[test]
    fn registry_source_rejects_signature_downgrade_across_default_host_spellings() {
        let dir = tempfile::tempdir().unwrap();
        let old_body = b"old registry wasm bytes";
        let old_shasum = sha256_bytes(old_body);
        let new_body = b"new registry wasm bytes";
        let new_shasum = sha256_bytes(new_body);
        let http = FakeRegistryHttp::default()
            .json(
                "https://registry.carina-rs.dev/.well-known/carina.json",
                r#"{"providers.v1":"/v1/providers/"}"#,
            )
            .json(
                "https://registry.carina-rs.dev/v1/providers/carina-rs/aws/versions",
                r#"{"sequence":7,"valid_until":"2999-01-01T00:00:00Z","versions":[{"version":"0.5.0","protocols":["1"]},{"version":"0.6.0","protocols":["1"]}]}"#,
            )
            .json(
                "https://registry.carina-rs.dev/v1/providers/carina-rs/aws/0.6.0/download",
                &format!(
                    r#"{{"protocols":["1"],"filename":"aws.wasm","download_url":"https://downloads.example.test/aws-0.6.0.wasm","shasum":"{new_shasum}","shasum_authored_by":"registry"}}"#
                ),
            )
            .downloadable_bytes("https://downloads.example.test/aws-0.6.0.wasm", new_body);
        let mut lock_file = LockFile::default();
        lock_file
            .upsert_registry(
                LockEntry {
                    name: "aws".into(),
                    source: "carina-rs/aws".into(),
                    kind: LockEntryKind::Version {
                        version: "0.5.0".into(),
                        constraint: None,
                    },
                    sha256: old_shasum,
                    registry: Some(RegistryLock {
                        resolved_hostname: "registry.carina-rs.dev".into(),
                        sequence: RegistrySequence::Present(7),
                        sequence_anchor: RegistrySequenceAnchor::Established(7),
                        valid_until_present: true,
                        yanked_versions: YankedRegistryVersions::default(),
                        signature: signed_fixture_pin(),
                        transparency_log_present: false,
                    }),
                },
                registry_host_lock("https://registry.carina-rs.dev/v1/providers/"),
            )
            .unwrap();

        let err = resolve_provider_with_http(
            dir.path(),
            "registry.carina-rs.dev/carina-rs/aws",
            "0.6.0",
            "aws",
            &mut lock_file,
            &http,
        )
        .unwrap_err();
        assert!(err.contains("signature"), "{err}");
    }

    #[test]
    fn registry_source_migrates_existing_explicit_default_host_lock_entry() {
        let mut lock_file = LockFile::default();
        lock_file.upsert(version_entry(
            "registry.carina-rs.dev/carina-rs/aws",
            "0.5.0",
        ));

        assert!(
            lock_file.find_by_source("carina-rs/aws").is_some(),
            "bare default-host spelling must find old explicit-default lock entries"
        );

        lock_file.upsert(LockEntry {
            name: "aws".into(),
            source: "carina-rs/aws".into(),
            kind: LockEntryKind::Version {
                version: "0.6.0".into(),
                constraint: None,
            },
            sha256: "def".into(),
            registry: None,
        });

        assert_eq!(lock_file.provider.len(), 1);
        assert_eq!(lock_file.provider[0].source, "carina-rs/aws");
    }

    #[test]
    fn registry_default_host_install_and_find_share_canonical_cache_path() {
        for (installed_source, requested_source) in [
            ("registry.carina-rs.dev/carina-rs/aws", "carina-rs/aws"),
            ("carina-rs/aws", "registry.carina-rs.dev/carina-rs/aws"),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let body = format!("registry wasm bytes for {installed_source}");
            let shasum = sha256_bytes(body.as_bytes());
            let http = registry_http(body.as_bytes(), &shasum);
            let mut lock_file = LockFile::default();

            let installed_path = resolve_provider_with_http(
                dir.path(),
                installed_source,
                "0.5.0",
                "aws",
                &mut lock_file,
                &http,
            )
            .unwrap();
            lock_file
                .save(&dir.path().join("carina-providers.lock"))
                .unwrap();

            let found_path =
                find_installed_provider(dir.path(), &provider_config(requested_source, None))
                    .unwrap();

            assert_eq!(found_path.path(), installed_path);
            assert_eq!(
                installed_path,
                cache_path_wasm(dir.path(), "carina-rs/aws", "0.5.0")
            );
            assert_eq!(lock_file.provider.len(), 1);
            assert_eq!(lock_file.provider[0].source, "carina-rs/aws");
        }
    }

    #[test]
    fn registry_default_host_resolve_single_writes_constraint_to_canonical_entry() {
        for (installed_source, requested_source) in [
            ("registry.carina-rs.dev/carina-rs/aws", "carina-rs/aws"),
            ("carina-rs/aws", "registry.carina-rs.dev/carina-rs/aws"),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let body = format!("registry wasm bytes for {installed_source}");
            let shasum = sha256_bytes(body.as_bytes());
            let http = registry_http(body.as_bytes(), &shasum);
            let mut lock_file = LockFile::default();

            let installed_path = resolve_provider_with_http(
                dir.path(),
                installed_source,
                "0.5.0",
                "aws",
                &mut lock_file,
                &http,
            )
            .unwrap();
            lock_file
                .save(&dir.path().join("carina-providers.lock"))
                .unwrap();

            let resolved_path = resolve_single_config_with_http(
                dir.path(),
                &versioned_config(requested_source, "~0.5.0"),
                &http,
            )
            .unwrap();
            let saved_lock = LockFile::load(&dir.path().join("carina-providers.lock"))
                .unwrap()
                .unwrap();

            assert_eq!(resolved_path, installed_path);
            assert_eq!(saved_lock.provider.len(), 1);
            assert_eq!(saved_lock.provider[0].source, "carina-rs/aws");
            assert!(
                matches!(
                    &saved_lock.provider[0].kind,
                    LockEntryKind::Version { constraint: Some(constraint), .. }
                        if constraint == "~0.5.0"
                ),
                "constraint must be written back through the canonical source"
            );
        }
    }

    #[test]
    fn registry_default_host_resolve_all_writes_constraint_to_canonical_entry() {
        for (installed_source, requested_source) in [
            ("registry.carina-rs.dev/carina-rs/aws", "carina-rs/aws"),
            ("carina-rs/aws", "registry.carina-rs.dev/carina-rs/aws"),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let body = format!("registry wasm bytes for {installed_source}");
            let shasum = sha256_bytes(body.as_bytes());
            let http = registry_http(body.as_bytes(), &shasum);
            let mut lock_file = LockFile::default();

            let installed_path = resolve_provider_with_http(
                dir.path(),
                installed_source,
                "0.5.0",
                "aws",
                &mut lock_file,
                &http,
            )
            .unwrap();
            lock_file
                .save(&dir.path().join("carina-providers.lock"))
                .unwrap();

            let resolved = resolve_all_with_http(
                dir.path(),
                &[versioned_config(requested_source, "~0.5.0")],
                LockMode::Normal,
                &http,
            )
            .unwrap();
            let saved_lock = LockFile::load(&dir.path().join("carina-providers.lock"))
                .unwrap()
                .unwrap();

            assert_eq!(resolved.get("awscc"), Some(&installed_path));
            assert_eq!(saved_lock.provider.len(), 1);
            assert_eq!(saved_lock.provider[0].source, "carina-rs/aws");
            assert!(
                matches!(
                    &saved_lock.provider[0].kind,
                    LockEntryKind::Version { constraint: Some(constraint), .. }
                        if constraint == "~0.5.0"
                ),
                "constraint must be written back through the canonical source"
            );
        }
    }

    #[test]
    fn registry_source_rejects_missing_previously_present_transparency_log() {
        let dir = tempfile::tempdir().unwrap();
        let body = b"registry wasm bytes";
        let shasum = sha256_bytes(body);
        let http = registry_http(body, &shasum).json(
            "https://registry.carina-rs.dev/v1/providers/carina-rs/aws/0.5.0/download",
            &format!(
                r#"{{"protocols":["1"],"filename":"aws.wasm","download_url":"https://downloads.example.test/aws.wasm","shasum":"{shasum}","shasum_authored_by":"registry","signature":{{"type":"sigstore-bundle","certificate_identity":"identity","certificate_oidc_issuer":"issuer","bundle_url":"https://downloads.example.test/bundle.sigstore.json"}}}}"#
            ),
        );
        let mut lock_file = LockFile::default();
        lock_file
            .upsert_registry(
                LockEntry {
                    name: "aws".into(),
                    source: "carina-rs/aws".into(),
                    kind: LockEntryKind::Version {
                        version: "0.5.0".into(),
                        constraint: None,
                    },
                    sha256: shasum,
                    registry: Some(RegistryLock {
                        resolved_hostname: "registry.carina-rs.dev".into(),
                        sequence: RegistrySequence::Present(7),
                        sequence_anchor: RegistrySequenceAnchor::Established(7),
                        valid_until_present: true,
                        yanked_versions: YankedRegistryVersions::default(),
                        signature: signature_pin("identity", "issuer"),
                        transparency_log_present: true,
                    }),
                },
                registry_host_lock("https://registry.carina-rs.dev/v1/providers/"),
            )
            .unwrap();

        let err = resolve_provider_with_http(
            dir.path(),
            "carina-rs/aws",
            "0.5.0",
            "aws",
            &mut lock_file,
            &http,
        )
        .unwrap_err();
        assert!(err.contains("transparency_log"), "{err}");
    }

    #[test]
    fn registry_source_rejects_expired_valid_until() {
        let dir = tempfile::tempdir().unwrap();
        let body = b"registry wasm bytes";
        let shasum = sha256_bytes(body);
        let http = registry_http(body, &shasum).json(
            "https://registry.carina-rs.dev/v1/providers/carina-rs/aws/versions",
            r#"{"sequence":7,"valid_until":"2000-01-01T00:00:00Z","versions":[{"version":"0.5.0","protocols":["1"]}]}"#,
        );
        let mut lock_file = LockFile::default();

        let err = resolve_provider_with_http(
            dir.path(),
            "carina-rs/aws",
            "0.5.0",
            "aws",
            &mut lock_file,
            &http,
        )
        .unwrap_err();
        assert!(err.contains("valid_until"), "{err}");
    }

    #[test]
    fn registry_source_rejects_default_discovery_cross_origin_base() {
        let dir = tempfile::tempdir().unwrap();
        let http = FakeRegistryHttp::default().json(
            "https://registry.carina-rs.dev/.well-known/carina.json",
            r#"{"providers.v1":"https://evil.example.test/v1/providers/"}"#,
        );
        let mut lock_file = LockFile::default();

        let err = resolve_provider_with_http(
            dir.path(),
            "carina-rs/aws",
            "0.5.0",
            "aws",
            &mut lock_file,
            &http,
        )
        .unwrap_err();
        assert!(err.contains("cross-origin"), "{err}");
    }

    #[test]
    fn registry_source_rejects_non_default_protocol_relative_cross_origin_base() {
        let error = resolve_api_base_url_for_host("registry.example.test", "//evil.test/a/")
            .expect_err("a discovery document must not relocate any registry across origins");

        assert_eq!(
            error,
            "registry discovery returned cross-origin providers.v1: //evil.test/a/"
        );
    }

    /// RFC 3986 resolves a directory-relative reference beneath `/.well-known/`.
    /// Registries that intend an origin-rooted API base should publish a rooted
    /// reference such as `/v1/providers/` instead.
    #[test]
    fn registry_discovery_resolves_references_against_the_original_well_known_url() {
        assert_eq!(
            resolve_api_base_url_for_host("registry.example.test", "v1/providers/").unwrap(),
            "https://registry.example.test/.well-known/v1/providers/"
        );
        assert_eq!(
            resolve_api_base_url_for_host("registry.example.test", "../v1/providers/").unwrap(),
            "https://registry.example.test/v1/providers/"
        );
    }

    #[test]
    fn registry_discovery_rejects_empty_query_or_fragment_only_api_base() {
        for providers_v1 in ["", "?version=1", "#fragment"] {
            let error = resolve_api_base_url_for_host("registry.example.test", providers_v1)
                .expect_err("the discovery document itself cannot be an API base");

            assert!(error.contains("discovery document"), "{error}");
        }
    }

    #[test]
    fn registry_discovery_rejects_api_base_with_query_or_fragment() {
        for providers_v1 in ["/v1/providers/?token=abc", "/v1/providers/#fragment"] {
            let error = resolve_api_base_url_for_host("registry.example.test", providers_v1)
                .expect_err("an API base cannot carry a query or fragment");

            assert!(error.contains("query or fragment"), "{error}");
        }
    }

    #[test]
    fn registry_discovery_rejects_api_base_with_userinfo() {
        let error = resolve_api_base_url_for_host(
            "registry.example.test",
            "https://user:pass@registry.example.test/v1/providers/",
        )
        .expect_err("an API base cannot carry credentials");

        assert!(error.contains("userinfo"), "{error}");
        assert!(!error.contains("user:pass"), "{error}");
    }

    #[test]
    fn registry_discovery_rejects_hostname_with_url_components() {
        for hostname in [
            "registry.example.test?x",
            "registry.example.test#fragment",
            "registry.example.test/path",
            "user@registry.example.test",
            "https://registry.example.test",
        ] {
            let error = resolve_api_base_url_for_host(hostname, "/v1/providers/")
                .expect_err("a registry hostname must not carry URL components");

            assert!(error.contains("invalid registry hostname"), "{error}");
        }
    }

    #[test]
    fn registry_discovery_rejects_discovery_document_as_api_base() {
        let error = resolve_api_base_url_for_host(
            "registry.example.test",
            "https://registry.example.test/.well-known/carina.json",
        )
        .expect_err("the discovery document itself cannot be an API base");

        assert!(error.contains("discovery document"), "{error}");
    }

    #[test]
    fn registry_discovery_normalizes_api_base_trailing_slash_before_pinning() {
        let without_slash =
            resolve_api_base_url_for_host("registry.example.test", "/v1/providers").unwrap();
        let with_slash =
            resolve_api_base_url_for_host("registry.example.test", "/v1/providers/").unwrap();

        assert_eq!(without_slash, with_slash);
        assert_eq!(with_slash, "https://registry.example.test/v1/providers/");
    }

    #[test]
    fn registry_source_treats_hostname_case_insensitively() {
        let dir = tempfile::tempdir().unwrap();
        let http = FakeRegistryHttp::default().json(
            "https://registry.carina-rs.dev/.well-known/carina.json",
            r#"{"providers.v1":"https://evil.example.test/v1/providers/"}"#,
        );
        let mut lock_file = LockFile::default();

        let err = resolve_provider_with_http(
            dir.path(),
            "Registry.Carina-RS.dev/carina-rs/aws",
            "0.5.0",
            "aws",
            &mut lock_file,
            &http,
        )
        .unwrap_err();
        assert!(err.contains("cross-origin"), "{err}");
    }

    #[test]
    fn registry_source_streams_large_wasm_instead_of_using_capped_get() {
        let dir = tempfile::tempdir().unwrap();
        let body = vec![b'w'; 10 * 1024 * 1024 + 1];
        let shasum = sha256_bytes(&body);
        let http = FakeRegistryHttp::default()
            .json(
                "https://registry.carina-rs.dev/.well-known/carina.json",
                r#"{"providers.v1":"/v1/providers/"}"#,
            )
            .json(
                "https://registry.carina-rs.dev/v1/providers/carina-rs/aws/versions",
                r#"{"sequence":7,"valid_until":"2999-01-01T00:00:00Z","versions":[{"version":"0.5.0","protocols":["1"]}]}"#,
            )
            .json(
                "https://registry.carina-rs.dev/v1/providers/carina-rs/aws/0.5.0/download",
                &format!(
                    r#"{{"protocols":["1"],"filename":"aws.wasm","download_url":"https://downloads.example.test/large-aws.wasm","shasum":"{shasum}","shasum_authored_by":"registry"}}"#
                ),
            )
            .downloadable_bytes("https://downloads.example.test/large-aws.wasm", &body);
        let mut lock_file = LockFile::default();

        let path = resolve_provider_with_http(
            dir.path(),
            "carina-rs/aws",
            "0.5.0",
            "aws",
            &mut lock_file,
            &http,
        )
        .expect("registry WASM download should stream to disk instead of using capped get");

        assert_eq!(sha256_file(&path).unwrap(), shasum);
        assert_eq!(fs::metadata(&path).unwrap().len(), body.len() as u64);
    }

    /// Serialize env-var tests in this module. `CARINA_PLUGIN_CACHE_DIR` is
    /// process-wide state and cargo test runs threads, so tests that touch it
    /// must hold this lock for their whole body.
    fn env_lock() -> &'static std::sync::Mutex<()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
    }

    /// Issue #2018: a lock file + global-cache hit must not mask a missing
    /// local `.carina/`. The project-local directory is the source of truth.
    #[test]
    fn find_installed_provider_revision_requires_local_install_not_global_cache() {
        let _guard = env_lock().lock().unwrap_or_else(|p| p.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path();

        let global = tmp.path().join("_global_cache");
        // SAFETY: env_lock() above serializes access with any other test that
        // touches CARINA_PLUGIN_CACHE_DIR in this process.
        unsafe { std::env::set_var("CARINA_PLUGIN_CACHE_DIR", &global) };

        let source = "github.com/carina-rs/carina-provider-awscc";
        let sha = "deadbeefcafe1234567890";

        let lock_path = base.join("carina-providers.lock");
        let mut lock = LockFile::default();
        lock.upsert(revision_entry(source, "main", sha));
        lock.save(&lock_path).unwrap();

        let global_wasm =
            crate::revision_resolver::global_cache_path_revision(source, sha).unwrap();
        fs::create_dir_all(global_wasm.parent().unwrap()).unwrap();
        fs::File::create(&global_wasm)
            .unwrap()
            .write_all(b"fake wasm from a prior project")
            .unwrap();

        let config = provider_config(source, Some("main"));
        let err = find_installed_provider(base, &config)
            .expect_err("missing local .carina/ must not be masked by a global-cache hit");
        assert!(err.contains("carina init"), "got: {err}");

        // SAFETY: still holding env_lock.
        unsafe { std::env::remove_var("CARINA_PLUGIN_CACHE_DIR") };
    }

    #[test]
    fn find_installed_provider_version_requires_local_install_not_global_cache() {
        let _guard = env_lock().lock().unwrap_or_else(|p| p.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path();

        let global = tmp.path().join("_global_cache");
        // SAFETY: env_lock() serializes.
        unsafe { std::env::set_var("CARINA_PLUGIN_CACHE_DIR", &global) };

        let source = "github.com/carina-rs/carina-provider-awscc";
        let version = "0.1.0";

        let lock_path = base.join("carina-providers.lock");
        let mut lock = LockFile::default();
        lock.upsert(version_entry(source, version));
        lock.save(&lock_path).unwrap();

        let global_wasm = global_cache_path_wasm(source, version).unwrap();
        fs::create_dir_all(global_wasm.parent().unwrap()).unwrap();
        fs::File::create(&global_wasm)
            .unwrap()
            .write_all(b"fake wasm from a prior project")
            .unwrap();

        let config = provider_config(source, None);
        let err = find_installed_provider(base, &config)
            .expect_err("missing local .carina/ must not be masked by a global-cache hit");
        assert!(err.contains("carina init"), "got: {err}");

        // SAFETY: still holding env_lock.
        unsafe { std::env::remove_var("CARINA_PLUGIN_CACHE_DIR") };
    }

    #[test]
    fn missing_revision_artifact_reports_the_consulted_lock_pin() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path();
        let source = "github.com/carina-rs/carina-provider-awscc";
        let revision = "main";
        let resolved_sha = "deadbeefcafe1234567890";
        let lock_path = base.join("carina-providers.lock");
        let mut lock = LockFile::default();
        lock.upsert(revision_entry(source, revision, resolved_sha));
        lock.save(&lock_path).unwrap();

        let error = find_installed_provider(base, &provider_config(source, Some(revision)))
            .expect_err("the locked revision artifact was deliberately not installed");
        let rendered = error.to_string();

        assert!(rendered.contains("Run `carina init`"), "{rendered}");
        assert!(
            rendered.contains(&lock_path.display().to_string()),
            "{rendered}"
        );
        assert!(rendered.contains("revision main"), "{rendered}");
        assert!(
            rendered.contains("resolved_sha deadbeefcafe1234567890"),
            "{rendered}"
        );
        assert!(!rendered.contains("lock is stale"), "{rendered}");
        assert!(!rendered.contains("carina init --upgrade"), "{rendered}");
    }

    #[test]
    fn missing_version_artifact_reports_the_consulted_lock_pin() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path();
        let source = "github.com/carina-rs/carina-provider-awscc";
        let version = "1.2.3";
        let constraint = "^1.0";
        let lock_path = base.join("carina-providers.lock");
        let mut lock = LockFile::default();
        lock.upsert(LockEntry {
            name: "awscc".into(),
            source: source.into(),
            kind: LockEntryKind::Version {
                version: version.into(),
                constraint: Some(constraint.into()),
            },
            sha256: "abc".into(),
            registry: None,
        });
        lock.save(&lock_path).unwrap();

        let error = find_installed_provider(base, &provider_config(source, None))
            .expect_err("the locked version artifact was deliberately not installed");
        let rendered = error.to_string();

        assert!(rendered.contains("Run `carina init`"), "{rendered}");
        assert!(
            rendered.contains(&lock_path.display().to_string()),
            "{rendered}"
        );
        assert!(rendered.contains("version 1.2.3"), "{rendered}");
        assert!(rendered.contains("constraint ^1.0"), "{rendered}");
        assert!(!rendered.contains("lock is stale"), "{rendered}");
        assert!(!rendered.contains("carina init --upgrade"), "{rendered}");
    }

    #[test]
    fn locked_version_provenance_renders_with_and_without_a_constraint() {
        let lock_path = PathBuf::from("project/carina-providers.lock");
        let render = |constraint: Option<&str>| {
            ProviderArtifactProvenance::LockFile {
                lock_path: lock_path.clone(),
                pin: LockedProviderPin::Version {
                    version: "1.2.3".into(),
                    constraint: constraint.map(str::to_string),
                },
            }
            .to_string()
        };

        assert_eq!(
            render(None),
            "provider resolved from project/carina-providers.lock (version 1.2.3); if this lock is stale, run `carina init --upgrade`"
        );
        assert_eq!(
            render(Some("^1.0")),
            "provider resolved from project/carina-providers.lock (version 1.2.3, constraint ^1.0); if this lock is stale, run `carina init --upgrade`"
        );
    }

    #[test]
    fn locked_registry_revision_provenance_renders_revision_and_version() {
        let provenance = ProviderArtifactProvenance::LockFile {
            lock_path: PathBuf::from("project/carina-providers.lock"),
            pin: LockedProviderPin::RegistryRevision {
                revision: "main".into(),
                version: "0.0.0-main.10.bbb".into(),
            },
        };

        assert_eq!(
            provenance.to_string(),
            "provider resolved from project/carina-providers.lock (registry revision main, version 0.0.0-main.10.bbb); if this lock is stale, run `carina init --upgrade`"
        );
    }

    #[test]
    fn find_installed_provider_registry_revision_uses_version_cache_path() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path();
        let source = "carina-rs/aws";
        let version = "0.0.0-main.10.bbb";
        let wasm_path = cache_path_wasm(base, source, version);
        fs::create_dir_all(wasm_path.parent().unwrap()).unwrap();
        fs::write(&wasm_path, b"registry revision wasm").unwrap();

        let mut lock = LockFile::default();
        lock.upsert(LockEntry {
            name: "aws".into(),
            source: source.into(),
            kind: LockEntryKind::RegistryRevision {
                revision: "main".into(),
                version: version.into(),
            },
            sha256: "abc".into(),
            registry: None,
        });
        lock.save(&base.join("carina-providers.lock")).unwrap();
        let config = provider_config(source, Some("main"));

        assert_eq!(
            find_installed_provider(base, &config).unwrap().path(),
            wasm_path
        );
    }

    #[test]
    fn revision_install_load_error_carries_stale_lock_provenance() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path();
        let source = "github.com/carina-rs/carina-provider-awscc";
        let revision = "main";
        let resolved_sha = "deadbeefcafe1234567890";
        let wasm_path = crate::revision_resolver::cache_path_revision(base, source, resolved_sha);
        fs::create_dir_all(wasm_path.parent().unwrap()).unwrap();
        fs::write(&wasm_path, b"stale provider wasm").unwrap();

        let mut lock = LockFile::default();
        lock.upsert(revision_entry(source, revision, resolved_sha));
        lock.save(&base.join("carina-providers.lock")).unwrap();

        let installed = find_installed_provider(base, &provider_config(source, Some(revision)))
            .expect("revision provider should resolve");
        assert_eq!(installed.path(), wasm_path);
        let error = installed.with_load_error(io::Error::other(
            "Failed to instantiate WASM component (HTTP)",
        ));
        assert!(
            std::error::Error::source(&error).is_none(),
            "the self-contained Display contract must not expose the rendered child again"
        );
        let rendered = error.to_string();

        assert!(rendered.starts_with("Failed to instantiate WASM component (HTTP)"));
        assert!(rendered.contains("provider resolved from"));
        assert!(rendered.contains("carina-providers.lock"));
        assert!(rendered.contains("revision main"));
        assert!(rendered.contains("resolved_sha deadbeefcafe1234567890"));
        assert!(rendered.contains("if this lock is stale"));
        assert!(rendered.contains("carina init --upgrade"));
    }

    #[test]
    fn file_install_load_error_cannot_claim_the_provider_lock_is_stale() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path();
        let source_path = base.join("local-provider.wasm");
        fs::write(&source_path, b"local provider wasm").unwrap();
        let source = format!("file://{}", source_path.display());
        let installed_path = base.join(".carina/providers/file/local-provider/local-provider.wasm");
        fs::create_dir_all(installed_path.parent().unwrap()).unwrap();
        fs::write(&installed_path, b"installed local provider wasm").unwrap();

        let installed = find_installed_provider(base, &provider_config(&source, None))
            .expect("file provider should resolve");
        assert_eq!(installed.path(), installed_path);
        let rendered = installed
            .with_load_error("Failed to instantiate WASM component (HTTP)")
            .to_string();

        assert!(rendered.contains(&format!("provider resolved from {source}")));
        assert!(rendered.contains("not controlled by carina-providers.lock"));
        assert!(!rendered.contains("if this lock is stale"));
        assert!(!rendered.contains("carina init --upgrade"));
    }

    // --- Issue #2026: lock vs .crn mismatch must error without --upgrade ---

    fn versioned_config(source: &str, constraint: &str) -> ProviderConfig {
        ProviderConfig {
            name: "awscc".into(),
            source: Some(source.into()),
            version: Some(
                carina_core::version_constraint::VersionConstraint::parse(constraint).unwrap(),
            ),
            revision: None,
            unresolved_attributes: IndexMap::new(),
            binding: None,
            is_default: true,
            attributes: IndexMap::new(),
            default_tags: IndexMap::new(),
        }
    }

    const SRC: &str = "github.com/carina-rs/carina-provider-awscc";

    #[test]
    fn check_mismatch_detects_constraint_unsatisfied() {
        let mut lock = LockFile::default();
        lock.upsert(version_entry(SRC, "0.5.2"));
        let cfg = versioned_config(SRC, "~0.6.0");

        let err = check_lock_mismatch(&[cfg], &lock, LockMode::Normal)
            .expect_err("lock version 0.5.2 does not satisfy ~0.6.0 — must error");
        assert!(err.contains("awscc"), "{err}");
        assert!(err.contains("0.5.2"), "{err}");
        assert!(err.contains("~0.6.0"), "{err}");
        assert!(err.contains("--upgrade"), "{err}");
    }

    #[test]
    fn check_mismatch_detects_version_to_revision_switch() {
        let mut lock = LockFile::default();
        lock.upsert(version_entry(SRC, "0.5.2"));
        let cfg = provider_config(SRC, Some("main"));

        let err = check_lock_mismatch(&[cfg], &lock, LockMode::Normal)
            .expect_err(".crn revision vs lock version must error");
        assert!(err.contains("awscc"), "{err}");
        assert!(err.contains("revision"), "{err}");
        assert!(err.contains("version"), "{err}");
        assert!(err.contains("--upgrade"), "{err}");
    }

    #[test]
    fn check_mismatch_accepts_matching_registry_revision_version_lock() {
        let source = "carina-rs/aws";
        let mut lock = LockFile::default();
        lock.upsert(LockEntry {
            name: "aws".into(),
            source: source.into(),
            kind: LockEntryKind::RegistryRevision {
                revision: "main".into(),
                version: "0.0.0-main.1.aaa".into(),
            },
            sha256: "abc".into(),
            registry: None,
        });
        let cfg = provider_config(source, Some("main"));

        assert!(check_lock_mismatch(&[cfg], &lock, LockMode::Normal).is_ok());
    }

    #[test]
    fn check_mismatch_detects_registry_revision_change_in_version_lock() {
        let source = "carina-rs/aws";
        let mut lock = LockFile::default();
        lock.upsert(LockEntry {
            name: "aws".into(),
            source: source.into(),
            kind: LockEntryKind::RegistryRevision {
                revision: "dev".into(),
                version: "0.0.0-dev.1.aaa".into(),
            },
            sha256: "abc".into(),
            registry: None,
        });
        let cfg = provider_config(source, Some("main"));

        let err = check_lock_mismatch(&[cfg], &lock, LockMode::Normal)
            .expect_err(".crn registry revision changed vs lock — must error");
        assert!(err.contains("revision = 'dev'"), "{err}");
        assert!(err.contains("revision = 'main'"), "{err}");
    }

    #[test]
    fn check_mismatch_detects_registry_revision_lock_when_revision_dropped_from_crn() {
        let source = "carina-rs/aws";
        let mut lock = LockFile::default();
        lock.upsert(LockEntry {
            name: "aws".into(),
            source: source.into(),
            kind: LockEntryKind::RegistryRevision {
                revision: "main".into(),
                version: "0.0.0-main.10.bbb".into(),
            },
            sha256: "abc".into(),
            registry: None,
        });
        let cfg = provider_config(source, None);

        let err = check_lock_mismatch(&[cfg], &lock, LockMode::Normal)
            .expect_err(".crn dropped registry revision but lock still has one - must error");
        assert!(err.contains("aws"), "{err}");
        assert!(err.contains("revision = 'main'"), "{err}");
        assert!(err.contains("(no revision, no version constraint"), "{err}");
        assert!(err.contains("--upgrade"), "{err}");
    }

    #[test]
    fn check_mismatch_accepts_plain_version_lock_when_crn_has_no_revision_or_version() {
        let source = "carina-rs/aws";
        let mut lock = LockFile::default();
        lock.upsert(version_entry(source, "0.5.0"));
        let cfg = provider_config(source, None);

        assert!(check_lock_mismatch(&[cfg], &lock, LockMode::Normal).is_ok());
    }

    #[test]
    fn check_mismatch_detects_revision_to_version_switch() {
        let mut lock = LockFile::default();
        lock.upsert(revision_entry(SRC, "main", "abc123"));
        let cfg = versioned_config(SRC, "~0.5.0");

        let err = check_lock_mismatch(&[cfg], &lock, LockMode::Normal)
            .expect_err(".crn version vs lock revision must error");
        assert!(err.contains("awscc"), "{err}");
        assert!(err.contains("--upgrade"), "{err}");
    }

    #[test]
    fn check_mismatch_detects_revision_change() {
        let mut lock = LockFile::default();
        lock.upsert(revision_entry(SRC, "main", "abc123"));
        let cfg = provider_config(SRC, Some("develop"));

        let err = check_lock_mismatch(&[cfg], &lock, LockMode::Normal)
            .expect_err(".crn revision changed vs lock — must error");
        assert!(err.contains("awscc"), "{err}");
        assert!(err.contains("main"), "{err}");
        assert!(err.contains("develop"), "{err}");
        assert!(err.contains("--upgrade"), "{err}");
    }

    /// Adding a new provider not in the lock is fine in Normal mode — that's
    /// the expected first-time flow.
    #[test]
    fn check_mismatch_allows_new_provider_in_normal_mode() {
        let lock = LockFile::default();
        let cfg = provider_config(SRC, Some("main"));
        assert!(check_lock_mismatch(&[cfg], &lock, LockMode::Normal).is_ok());
    }

    /// In `--locked` mode, a provider missing from the lock is an error (the
    /// lock is supposed to be the full source of truth, matching `cargo --locked`).
    #[test]
    fn check_mismatch_rejects_new_provider_in_locked_mode() {
        let lock = LockFile::default();
        let cfg = provider_config(SRC, Some("main"));

        let err = check_lock_mismatch(&[cfg], &lock, LockMode::Locked)
            .expect_err("--locked must error when a provider is missing from the lock");
        assert!(err.contains("awscc"), "{err}");
        assert!(err.contains("locked"), "{err}");
    }

    /// Happy path: lock matches .crn exactly → no error.
    #[test]
    fn check_mismatch_accepts_matching_version() {
        let mut lock = LockFile::default();
        lock.upsert(version_entry(SRC, "0.5.2"));
        let cfg = versioned_config(SRC, "~0.5.0");

        assert!(check_lock_mismatch(&[cfg], &lock, LockMode::Normal).is_ok());
    }

    #[test]
    fn check_mismatch_accepts_matching_revision() {
        let mut lock = LockFile::default();
        lock.upsert(revision_entry(SRC, "main", "abc"));
        let cfg = provider_config(SRC, Some("main"));

        assert!(check_lock_mismatch(&[cfg], &lock, LockMode::Normal).is_ok());
    }

    /// .crn without a version constraint and lock with a pinned version is OK
    /// (no constraint means "accept whatever is locked").
    #[test]
    fn check_mismatch_accepts_unconstrained_version_config() {
        let mut lock = LockFile::default();
        lock.upsert(version_entry(SRC, "0.5.2"));
        let cfg = provider_config(SRC, None);

        assert!(check_lock_mismatch(&[cfg], &lock, LockMode::Normal).is_ok());
    }

    /// .crn without revision but lock in revision mode is a mismatch — the
    /// user dropped `revision = '...'` from their config and `.crn` now
    /// implies version mode, but the lock still pins a git revision.
    #[test]
    fn check_mismatch_detects_revision_dropped_from_crn() {
        let mut lock = LockFile::default();
        lock.upsert(revision_entry(SRC, "main", "abc123"));
        let cfg = provider_config(SRC, None); // no revision, no version

        let err = check_lock_mismatch(&[cfg], &lock, LockMode::Normal)
            .expect_err(".crn lost its revision but lock still has one — must error");
        assert!(err.contains("awscc"), "{err}");
        assert!(err.contains("main"), "{err}");
        assert!(err.contains("--upgrade"), "{err}");
    }

    /// End-to-end: `resolve_all` in Normal mode with a stale lock errors
    /// *before* doing any network or filesystem work, and leaves the existing
    /// lock file untouched. That's the invariant the whole fix is built on.
    #[test]
    fn resolve_all_errors_on_mismatch_without_touching_lock() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path();
        let lock_path = base.join("carina-providers.lock");

        // Pre-existing lock: revision mode.
        let mut lock = LockFile::default();
        lock.upsert(revision_entry(SRC, "main", "abc123"));
        lock.save(&lock_path).unwrap();
        let before = fs::read_to_string(&lock_path).unwrap();

        // .crn now wants a version — should error, not fall through to a
        // network fetch, and not rewrite the lock.
        let providers = vec![versioned_config(SRC, "~0.5.0")];
        let err = resolve_all(base, &providers, LockMode::Normal)
            .expect_err("mismatched lock must abort resolve_all");
        assert!(err.contains("--upgrade"), "{err}");

        let after = fs::read_to_string(&lock_path).unwrap();
        assert_eq!(before, after, "lock must be untouched on mismatch error");
    }

    /// file:// providers skip the lock-mismatch check — their `sha256` is
    /// refreshed on every `init` by design.
    #[test]
    fn check_mismatch_skips_file_sources() {
        let mut lock = LockFile::default();
        lock.upsert(LockEntry {
            name: "test".into(),
            source: "file:///tmp/provider.wasm".into(),
            kind: LockEntryKind::File,
            sha256: "abc".into(),
            registry: None,
        });
        let cfg = ProviderConfig {
            name: "test".into(),
            source: Some("file:///tmp/provider.wasm".into()),
            version: None,
            revision: None,
            unresolved_attributes: IndexMap::new(),
            binding: None,
            is_default: true,
            attributes: IndexMap::new(),
            default_tags: IndexMap::new(),
        };
        assert!(check_lock_mismatch(&[cfg], &lock, LockMode::Normal).is_ok());
    }
}
