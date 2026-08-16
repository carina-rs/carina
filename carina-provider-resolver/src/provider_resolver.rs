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
    pub resolved_hostname: String,
    pub api_base_url: String,
    pub discovery_sha256: String,
    /// The greatest fully validated sequence observed for this source. This is
    /// durable across downstream failures but is not a rollback floor.
    sequence: RegistrySequence,
    sequence_anchor: RegistrySequenceAnchor,
    valid_until_present: bool,
    yanked_versions: YankedRegistryVersions,
    signature: RegistrySignatureProtection,
    transparency_log_present: bool,
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

    fn insert(
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

    fn set(&mut self, source: String, ratchets: RegistryRatchets) {
        self.0.insert(source, ratchets);
    }

    fn remove(&mut self, source: &str) -> Option<RegistryRatchets> {
        self.0.remove(source)
    }

    fn into_canonical(self) -> Result<Self, RegistryIdentityPinConflict> {
        let mut canonical = Self::default();
        for (source, ratchets) in self.0 {
            canonical.insert(canonical_lock_source(&source), ratchets)?;
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

/// Signature protection recorded for a registry provider. Signed entries
/// always carry their identity pin; a signed-but-unpinned lock cannot exist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistrySignatureProtection {
    Absent,
    Present(IdentityPin),
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

impl Default for RegistryRatchets {
    fn default() -> Self {
        Self {
            sequence: RegistrySequence::Absent,
            valid_until_present: false,
            yanked_versions: YankedRegistryVersions::default(),
            signature: RegistrySignatureProtection::Absent,
            transparency_log_present: false,
        }
    }
}

impl RegistryRatchets {
    fn merge(mut self, other: &Self) -> Result<Self, RegistryIdentityPinConflict> {
        let signature = match (&self.signature, &other.signature) {
            (RegistrySignatureProtection::Absent, signature)
            | (signature, RegistrySignatureProtection::Absent) => signature.clone(),
            (
                RegistrySignatureProtection::Present(left),
                RegistrySignatureProtection::Present(right),
            ) if left == right => RegistrySignatureProtection::Present(left.clone()),
            (
                RegistrySignatureProtection::Present(left),
                RegistrySignatureProtection::Present(right),
            ) => {
                return Err(RegistryIdentityPinConflict {
                    left: left.clone(),
                    right: right.clone(),
                });
            }
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

    fn apply_to(self, registry: &mut RegistryLock) {
        registry.sequence = self.sequence;
        registry.valid_until_present = self.valid_until_present;
        registry.yanked_versions = self.yanked_versions;
        registry.signature = self.signature;
        registry.transparency_log_present = self.transparency_log_present;
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
        let signature = match (
            value.signature_present,
            value.certificate_identity,
            value.certificate_oidc_issuer,
        ) {
            (false, None, None) => RegistrySignatureProtection::Absent,
            (true, Some(certificate_identity), Some(certificate_oidc_issuer)) => {
                RegistrySignatureProtection::Present(IdentityPin {
                    certificate_identity,
                    certificate_oidc_issuer,
                })
            }
            _ => return Err(RegistryLockError::InconsistentSignature),
        };
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
            match value.signature {
                RegistrySignatureProtection::Absent => (false, None, None),
                RegistrySignatureProtection::Present(pin) => (
                    true,
                    Some(pin.certificate_identity),
                    Some(pin.certificate_oidc_issuer),
                ),
            };
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
    api_base_url: String,
    discovery_sha256: String,
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
        let signature = match (
            value.signature_present,
            value.certificate_identity,
            value.certificate_oidc_issuer,
        ) {
            (false, None, None) => RegistrySignatureProtection::Absent,
            (true, Some(certificate_identity), Some(certificate_oidc_issuer)) => {
                RegistrySignatureProtection::Present(IdentityPin {
                    certificate_identity,
                    certificate_oidc_issuer,
                })
            }
            _ => return Err(RegistryLockError::InconsistentSignature),
        };
        Ok(Self {
            resolved_hostname: value.resolved_hostname,
            api_base_url: value.api_base_url,
            discovery_sha256: value.discovery_sha256,
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
            match value.signature {
                RegistrySignatureProtection::Absent => (false, None, None),
                RegistrySignatureProtection::Present(pin) => (
                    true,
                    Some(pin.certificate_identity),
                    Some(pin.certificate_oidc_issuer),
                ),
            };
        Self {
            resolved_hostname: value.resolved_hostname,
            api_base_url: value.api_base_url,
            discovery_sha256: value.discovery_sha256,
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
                "registry lock is inconsistent: signature_present, certificate_identity, and certificate_oidc_issuer must either all describe a signature pin or all be absent"
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
    fn expected_identity(&self) -> Option<ExpectedIdentity> {
        match self {
            Self::Absent => None,
            Self::Present(pin) => Some(ExpectedIdentity::pinned(
                pin.certificate_identity.clone(),
                pin.certificate_oidc_issuer.clone(),
            )),
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

impl RegistryLock {
    fn from_resolved_registry(
        registry: ResolvedRegistry,
        ratchets: RegistryRatchets,
        validated_sequence: ValidatedRegistrySequence,
    ) -> Self {
        let ResolvedRegistry {
            hostname,
            api_base_url,
            discovery_sha256,
        } = registry;
        let RegistryRatchets {
            sequence,
            valid_until_present,
            yanked_versions,
            signature,
            transparency_log_present,
        } = ratchets;
        let sequence_anchor = validated_sequence.into_anchor();
        Self {
            resolved_hostname: hostname,
            api_base_url,
            discovery_sha256,
            sequence,
            sequence_anchor,
            valid_until_present,
            yanked_versions,
            signature,
            transparency_log_present,
        }
    }

    pub fn yanked_versions(&self) -> &YankedRegistryVersions {
        &self.yanked_versions
    }
}

/// The full carina-providers.lock file.
///
/// `LockFile` deliberately implements only `Serialize`. All deserialization is
/// routed through [`Self::load`], which checks the format version before the
/// current schema can consume or rewrite the file.
#[derive(Debug, Clone, Serialize)]
pub struct LockFile {
    version: u32,
    pub provider: Vec<LockEntry<RegistryLock>>,
    #[serde(default, skip_serializing_if = "UnpinnedRegistryRatchets::is_empty")]
    unpinned_registry_ratchets: UnpinnedRegistryRatchets,
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
            Self::MissingVersion { .. } | Self::VersionTooNew { .. } => None,
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
            provider: Vec::new(),
            unpinned_registry_ratchets: UnpinnedRegistryRatchets::default(),
        }
    }
}

impl LockFile {
    /// v1: Registry protection-presence fields are mandatory, and signed
    /// registry entries always carry a signing-identity pin. New protection
    /// fields remain in v1 when their absence has a safe, explicit default;
    /// older readers reject the unknown field instead of silently removing it.
    pub const CURRENT_VERSION: u32 = 1;

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
        let mut lock = Self {
            version: Self::CURRENT_VERSION,
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

    pub fn save(&self, path: &Path) -> io::Result<()> {
        let content = toml::to_string_pretty(self)
            .map_err(|e| io::Error::other(format!("Failed to serialize lock file: {e}")))?;
        fs::write(path, content)
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
                RegistryRatchets::from(&*registry)
                    .merge(&unpinned)?
                    .apply_to(registry);
            }
        }
        Ok(())
    }

    fn store_registry_ratchets(&mut self, source: &str, ratchets: RegistryRatchets) {
        let source_key = canonical_lock_source(source);
        if let Some(registry) = self
            .provider
            .iter_mut()
            .find(|entry| Self::sources_match(&entry.source, &source_key))
            .and_then(|entry| entry.registry.as_mut())
        {
            ratchets.apply_to(registry);
            self.unpinned_registry_ratchets.remove(&source_key);
        } else {
            self.unpinned_registry_ratchets.set(source_key, ratchets);
        }
    }

    pub fn upsert(&mut self, entry: LockEntry<NoRegistryLock>) {
        self.upsert_entry(entry.into_stored());
    }

    #[cfg(test)]
    fn upsert_registry(
        &mut self,
        mut entry: LockEntry<RegistryLock>,
    ) -> Result<(), RegistryIdentityPinConflict> {
        if let Some(registry) = entry.registry.as_mut() {
            let observed = self.known_registry_ratchets(&entry.source)?;
            let merged = RegistryRatchets::from(&*registry).merge(&observed)?;
            merged.apply_to(registry);
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
        persisted.store_registry_ratchets(&source_key, ratchets);
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
            signature: RegistrySignatureProtection::Present(pin),
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
        let ratchets = self
            .lock_file
            .known_registry_ratchets(&entry.source)
            .map_err(|error| error.to_string())?;
        let registry = RegistryLock::from_resolved_registry(
            entry.registry,
            ratchets,
            entry.validated_sequence,
        );
        self.lock_file.upsert_entry(LockEntry {
            name: entry.name,
            source: entry.source,
            kind: entry.kind,
            sha256: entry.sha256,
            registry: Some(registry),
        });
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
const IDENTITY_REPIN_REMEDIATION: &str = "After verifying out-of-band that this is intended (a legitimate signing-identity change or a deliberate downgrade to a pre-signing version), remove that provider's entry from carina-providers.lock and re-run carina init to re-pin.";

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
pub struct ResolvedRegistry {
    hostname: String,
    api_base_url: String,
    discovery_sha256: String,
}

#[derive(Debug, Clone)]
struct HttpResponse {
    status: u16,
    body: Vec<u8>,
}

trait RegistryHttp {
    fn get(&self, url: &str) -> Result<HttpResponse, String>;

    fn download_to_file(&self, url: &str, dest: &Path) -> Result<(), String> {
        let response = self.get(url)?;
        if response.status != 200 {
            return Err(format!(
                "Download failed with status {}: {url}",
                response.status
            ));
        }
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("Failed to create directory {}: {e}", parent.display()))?;
        }
        fs::write(dest, response.body)
            .map_err(|e| format!("Failed to write file {}: {e}", dest.display()))?;
        Ok(())
    }
}

struct UreqRegistryHttp;

impl RegistryHttp for UreqRegistryHttp {
    fn get(&self, url: &str) -> Result<HttpResponse, String> {
        let response = match ureq::get(url).call() {
            Ok(response) => response,
            Err(ureq::Error::StatusCode(status)) => {
                return Ok(HttpResponse {
                    status,
                    body: Vec::new(),
                });
            }
            Err(e) => return Err(format!("Failed to fetch {url}: {e}")),
        };
        let status = response.status().into();
        let body = response
            .into_body()
            .read_to_vec()
            .map_err(|e| format!("Failed to read response body from {url}: {e}"))?;
        Ok(HttpResponse { status, body })
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
                        return Err(format!(
                            "registry sequence rollback for {}/{}: previous {}, got {}",
                            source.namespace, source.name, previous, sequence
                        ));
                    }
                    if sequence.saturating_sub(previous) > MAX_SEQUENCE_FAST_FORWARD {
                        return Err(format!(
                            "registry sequence fast-forward for {}/{} is too large: established anchor {}, got {}",
                            source.namespace, source.name, previous, sequence
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
                signature: RegistrySignatureProtection::Absent,
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

fn sha256_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

fn fetch_json<T: for<'de> Deserialize<'de>, H: RegistryHttp>(
    http: &H,
    url: &str,
) -> Result<(T, Vec<u8>), String> {
    let response = http.get(url)?;
    if response.status != 200 {
        return Err(format!(
            "Registry request failed with status {}: {url}",
            response.status
        ));
    }
    let parsed = serde_json::from_slice(&response.body)
        .map_err(|e| format!("Failed to parse registry JSON from {url}: {e}"))?;
    Ok((parsed, response.body))
}

fn join_registry_url(base: &str, path: &str) -> String {
    format!("{}{}", base.trim_end_matches('/'), path)
}

fn resolve_registry<H: RegistryHttp>(
    source: &RegistrySource,
    http: &H,
) -> Result<ResolvedRegistry, String> {
    let discovery_url = format!("https://{}/.well-known/carina.json", source.hostname);
    let (discovery, body): (DiscoveryDocument, Vec<u8>) = fetch_json(http, &discovery_url)?;
    let api_base_url = resolve_api_base_url(&source.hostname, &discovery.providers_v1)?;
    Ok(ResolvedRegistry {
        hostname: source.hostname.clone(),
        api_base_url,
        discovery_sha256: sha256_bytes(&body),
    })
}

fn resolve_api_base_url(hostname: &str, providers_v1: &str) -> Result<String, String> {
    let origin = format!("https://{hostname}");
    if providers_v1.starts_with("https://") {
        if hostname == DEFAULT_REGISTRY_HOST
            && !providers_v1.starts_with(&format!("{origin}/"))
            && providers_v1 != origin
        {
            return Err(format!(
                "default registry discovery returned cross-origin providers.v1: {providers_v1}"
            ));
        }
        return Ok(ensure_trailing_slash(providers_v1));
    }
    if providers_v1.starts_with("http://") {
        return Err(format!(
            "registry discovery providers.v1 must use HTTPS: {providers_v1}"
        ));
    }
    if providers_v1.starts_with('/') {
        return Ok(ensure_trailing_slash(&format!("{origin}{providers_v1}")));
    }
    Ok(ensure_trailing_slash(&format!("{origin}/{providers_v1}")))
}

fn ensure_trailing_slash(url: &str) -> String {
    if url.ends_with('/') {
        url.to_string()
    } else {
        format!("{url}/")
    }
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
        &registry.api_base_url,
        &format!("/{}/{}/versions", source.namespace, source.name),
    );
    let (versions, _): (RegistryVersions, Vec<u8>) = fetch_json(http, &url)?;
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
        &registry.api_base_url,
        &format!("/{}/{}/{version}/download", source.namespace, source.name),
    );
    let (download, _): (RegistryDownload, Vec<u8>) = fetch_json(http, &url)?;
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
    if let Some(entry) = entry {
        if matches!(&entry.kind, LockEntryKind::Version { version: locked, .. } if locked == version)
            && entry.sha256 != expected_shasum
        {
            return Err(format!(
                "registry shasum pin mismatch for {}@{}: lock has {}, registry returned {}",
                source_key, version, entry.sha256, expected_shasum
            ));
        }
        if let Some(locked_registry) = &entry.registry {
            if locked_registry.resolved_hostname != registry.hostname {
                return Err(format!(
                    "registry hostname pin mismatch for {}: lock has {}, resolved {}",
                    source_key, locked_registry.resolved_hostname, registry.hostname
                ));
            }
            if locked_registry.api_base_url != registry.api_base_url {
                return Err(format!("registry API base pin mismatch for {source_key}"));
            }
            if locked_registry.discovery_sha256 != registry.discovery_sha256 {
                return Err(format!(
                    "registry discovery document pin mismatch for {source_key}"
                ));
            }
        }
    }
    let ratchets = current_lock
        .known_registry_ratchets(&source_key)
        .map_err(|error| error.to_string())?;
    let expected_identity = ratchets.signature.expected_identity();
    if expected_identity.is_some() && signature.is_none() {
        return Err(format!(
            "the resolved version of {source_key} has no registry signature, but carina-providers.lock records this provider as signed and identity-pinned; downgrades from signed to unsigned versions are refused and have no override. {IDENTITY_REPIN_REMEDIATION}"
        ));
    }
    if let (Some(expected_identity), Some(signature)) = (&expected_identity, signature) {
        let (certificate_identity, certificate_oidc_issuer) = expected_identity.values();
        if signature.certificate_identity != certificate_identity
            || signature.certificate_oidc_issuer != certificate_oidc_issuer
        {
            return Err(format!(
                "registry signature identity for {source_key} differs from the carina-providers.lock pin; signature verification has no override. {IDENTITY_REPIN_REMEDIATION}"
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
    let response = http.get(&signature.bundle_url).map_err(|error| {
        format!(
            "cannot fetch the signature bundle from {} ({error}); signature verification cannot proceed and has no override",
            signature.bundle_url
        )
    })?;
    if response.status != 200 {
        return Err(format!(
            "cannot fetch the signature bundle from {} (HTTP {}); signature verification cannot proceed and has no override",
            signature.bundle_url, response.status
        ));
    }
    if response.body.len() > MAX_SIGNATURE_BUNDLE_BYTES {
        return Err(signing::verification_failure(format!(
            "signature bundle from {} is {} bytes, exceeding the {MAX_SIGNATURE_BUNDLE_BYTES}-byte limit",
            signature.bundle_url,
            response.body.len()
        )));
    }
    Ok(response.body)
}

fn resolve_registry_provider_with_http<H: RegistryHttp>(
    base_dir: &Path,
    source: &RegistrySource,
    version: &str,
    name: &str,
    lock_file: &mut LockFile,
    http: &H,
) -> Result<PathBuf, String> {
    let registry = resolve_registry(source, http)?;
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
    let registry = resolve_registry(source, http)?;
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

    fn signature_pin(identity: &str, issuer: &str) -> RegistrySignatureProtection {
        RegistrySignatureProtection::Present(IdentityPin {
            certificate_identity: identity.into(),
            certificate_oidc_issuer: issuer.into(),
        })
    }

    fn signed_fixture_pin() -> RegistrySignatureProtection {
        signature_pin(SIGNED_FIXTURE_IDENTITY, SIGNED_FIXTURE_ISSUER)
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
        lock.upsert_registry(LockEntry {
            name: "aws".into(),
            source: "carina-rs/aws".into(),
            kind: LockEntryKind::Version {
                version: "0.5.0".into(),
                constraint: None,
            },
            sha256: "abc".into(),
            registry: Some(RegistryLock {
                resolved_hostname: "registry.carina-rs.dev".into(),
                api_base_url: "https://registry.carina-rs.dev/v1/providers/".into(),
                discovery_sha256: "def".into(),
                sequence: RegistrySequence::Present(7),
                sequence_anchor: RegistrySequenceAnchor::Established(7),
                valid_until_present: true,
                yanked_versions: YankedRegistryVersions::default(),
                signature: signed_fixture_pin(),
                transparency_log_present: true,
            }),
        })
        .unwrap();
        toml::to_string_pretty(&lock).unwrap()
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
        let toml_str = toml::to_string_pretty(&lock).unwrap();
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
        let toml_str = toml::to_string_pretty(&lock).unwrap();
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
            provider: vec![revision_entry(
                "github.com/carina-rs/carina-provider-awscc",
                "main",
                "81b6910fb34e84784daac2a02c915e821b2da570",
            )],
            unpinned_registry_ratchets: UnpinnedRegistryRatchets::default(),
        };
        let toml_str = toml::to_string_pretty(&lock).unwrap();
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
            provider: vec![LockEntry {
                name: "test".into(),
                source: "file:///tmp/my-provider.wasm".into(),
                kind: LockEntryKind::File,
                sha256: "abc".into(),
                registry: None,
            }],
            unpinned_registry_ratchets: UnpinnedRegistryRatchets::default(),
        };
        let toml_str = toml::to_string_pretty(&lock).unwrap();
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
            r#"version = 1

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
            r#"version = 1

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
    fn lock_file_roundtripped_through_cd228086_is_rejected() {
        let old_lock: Cd228086LockFile = toml::from_str(&fully_protected_lock_toml()).unwrap();
        let stripped = toml::to_string_pretty(&old_lock).unwrap();

        assert!(!stripped.contains("version = 1"), "{stripped}");
        assert!(!stripped.contains("certificate_identity"), "{stripped}");
        assert!(!stripped.contains("certificate_oidc_issuer"), "{stripped}");
        let error = lock_toml_error(&stripped);
        assert!(
            matches!(error, LockFileError::MissingVersion { .. }),
            "{error}"
        );
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
    fn lock_load_rejects_stripped_registry_identity_pin() {
        let stripped = fully_protected_lock_toml()
            .lines()
            .filter(|line| {
                !line.starts_with("certificate_identity")
                    && !line.starts_with("certificate_oidc_issuer")
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert_lock_toml_is_rejected(&stripped);
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
    fn existing_registry_locks_default_sticky_yanked_set_to_empty() {
        let serialized = fully_protected_lock_toml();
        assert!(serialized.starts_with("version = 1\n"), "{serialized}");
        assert!(!serialized.contains("yanked_versions"), "{serialized}");

        let lock =
            LockFile::from_toml_str(&serialized, Path::new("carina-providers.lock")).unwrap();
        let registry = lock.provider[0].registry.as_ref().unwrap();
        assert!(registry.yanked_versions().is_empty());
        assert!(
            toml::to_string_pretty(&lock)
                .unwrap()
                .starts_with("version = 1\n"),
            "adding a defaulted yank set must not force a lock-format bump"
        );
    }

    #[test]
    fn registry_identity_pin_toml_roundtrip_preserves_flat_bytes() {
        let lock = LockFile {
            version: LockFile::CURRENT_VERSION,
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
                    api_base_url: "https://registry.carina-rs.dev/v1/providers/".into(),
                    discovery_sha256: "def".into(),
                    sequence: RegistrySequence::Present(7),
                    sequence_anchor: RegistrySequenceAnchor::Established(7),
                    valid_until_present: true,
                    yanked_versions: YankedRegistryVersions::default(),
                    signature: RegistrySignatureProtection::Present(IdentityPin {
                        certificate_identity: SIGNED_FIXTURE_IDENTITY.into(),
                        certificate_oidc_issuer: SIGNED_FIXTURE_ISSUER.into(),
                    }),
                    transparency_log_present: false,
                }),
            }],
            unpinned_registry_ratchets: UnpinnedRegistryRatchets::default(),
        };
        let expected = format!(
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

        let serialized = toml::to_string_pretty(&lock).unwrap();
        assert_eq!(serialized, expected);
        let reparsed =
            LockFile::from_toml_str(&serialized, Path::new("carina-providers.lock")).unwrap();
        assert_eq!(toml::to_string_pretty(&reparsed).unwrap(), expected);
    }

    #[test]
    fn registry_ratchet_storage_load_attaches_unpinned_to_provider() {
        let serialized = r#"version = 1

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
            !toml::to_string_pretty(&lock)
                .unwrap()
                .contains("[unpinned_registry_ratchets."),
            "load-time attachment must leave each source in one persisted location"
        );
    }

    #[test]
    fn registry_ratchet_storage_store_clears_shadow_for_pinned_provider() {
        let source = "carina-rs/aws";
        let mut lock = LockFile::default();
        lock.upsert_registry(LockEntry {
            name: "aws".into(),
            source: source.into(),
            kind: LockEntryKind::Version {
                version: "0.5.0".into(),
                constraint: None,
            },
            sha256: "abc".into(),
            registry: Some(RegistryLock {
                resolved_hostname: "registry.carina-rs.dev".into(),
                api_base_url: "https://registry.carina-rs.dev/v1/providers/".into(),
                discovery_sha256: "def".into(),
                sequence: RegistrySequence::Absent,
                sequence_anchor: RegistrySequenceAnchor::Unestablished,
                valid_until_present: false,
                yanked_versions: YankedRegistryVersions::default(),
                signature: RegistrySignatureProtection::Absent,
                transparency_log_present: false,
            }),
        })
        .unwrap();
        let observed = RegistryRatchets {
            sequence: RegistrySequence::Present(7),
            valid_until_present: true,
            ..RegistryRatchets::default()
        };
        lock.unpinned_registry_ratchets
            .set(source.into(), observed.clone());

        lock.store_registry_ratchets(source, observed);

        assert!(lock.unpinned_registry_ratchets.get(source).is_none());
        let registry = lock.provider[0].registry.as_ref().unwrap();
        assert_eq!(registry.sequence.value(), Some(7));
        assert!(registry.valid_until_present);
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
        lock.upsert_registry(LockEntry {
            name: "aws".into(),
            source: "carina-rs/aws".into(),
            kind: LockEntryKind::Version {
                version: "0.5.0".into(),
                constraint: None,
            },
            sha256: "recorded".into(),
            registry: Some(RegistryLock {
                resolved_hostname: "registry.carina-rs.dev".into(),
                api_base_url: "https://registry.carina-rs.dev/v1/providers/".into(),
                discovery_sha256: "discovery".into(),
                sequence: RegistrySequence::Present(100),
                sequence_anchor: RegistrySequenceAnchor::Established(100),
                valid_until_present: true,
                yanked_versions: YankedRegistryVersions::default(),
                signature: signature_pin("id-a", "issuer-a"),
                transparency_log_present: true,
            }),
        })
        .unwrap();

        let hostile = lock
            .upsert_registry(LockEntry {
                name: "aws".into(),
                source: "carina-rs/aws".into(),
                kind: LockEntryKind::Version {
                    version: "0.6.0".into(),
                    constraint: None,
                },
                sha256: "proposed".into(),
                registry: Some(RegistryLock {
                    resolved_hostname: "registry.carina-rs.dev".into(),
                    api_base_url: "https://registry.carina-rs.dev/v1/providers/".into(),
                    discovery_sha256: "discovery".into(),
                    sequence: RegistrySequence::Absent,
                    sequence_anchor: RegistrySequenceAnchor::Unestablished,
                    valid_until_present: false,
                    yanked_versions: YankedRegistryVersions::default(),
                    signature: signature_pin("EVIL", "EVIL-issuer"),
                    transparency_log_present: false,
                }),
            })
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

    #[derive(Default)]
    struct FakeRegistryHttp {
        responses: HashMap<String, HttpResponse>,
        downloads: HashMap<String, Vec<u8>>,
        requested: std::sync::Mutex<Vec<String>>,
    }

    impl FakeRegistryHttp {
        fn response(mut self, url: &str, status: u16, body: &[u8]) -> Self {
            self.responses.insert(
                url.to_string(),
                HttpResponse {
                    status,
                    body: body.to_vec(),
                },
            );
            self
        }

        fn json(mut self, url: &str, body: &str) -> Self {
            self.responses.insert(
                url.to_string(),
                HttpResponse {
                    status: 200,
                    body: body.as_bytes().to_vec(),
                },
            );
            self
        }

        fn bytes(mut self, url: &str, body: &[u8]) -> Self {
            self.responses.insert(
                url.to_string(),
                HttpResponse {
                    status: 200,
                    body: body.to_vec(),
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
        fn get(&self, url: &str) -> Result<HttpResponse, String> {
            self.requested.lock().unwrap().push(url.to_string());
            self.responses
                .get(url)
                .cloned()
                .ok_or_else(|| format!("unexpected test URL: {url}"))
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
    fn registry_ratchet_storage_identity_repin_remediation_clears_baseline() {
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
            .expect("removing the provider TOML block must establish a fresh baseline");

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
        assert!(error.contains("identity-pinned"), "{error}");
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
        let RegistrySignatureProtection::Present(pin) = &registry.signature else {
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
        assert!(error.contains("re-run carina init to re-pin"), "{error}");
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
        assert!(error.contains("identity-pinned"), "{error}");
        assert!(error.contains("verifying out-of-band"), "{error}");
        assert!(error.contains("re-run carina init to re-pin"), "{error}");
        assert!(!unsigned_http.was_requested("https://downloads.example.test/aws.wasm"));
    }

    #[test]
    fn registry_source_reuses_existing_signature_identity_pin() {
        let dir = tempfile::tempdir().unwrap();
        let shasum = sha256_bytes(SIGNED_FIXTURE_ARTIFACT);
        let mut lock_file = LockFile::default();
        lock_file
            .upsert_registry(LockEntry {
                name: "aws".into(),
                source: "carina-rs/aws".into(),
                kind: LockEntryKind::Version {
                    version: "0.5.0".into(),
                    constraint: None,
                },
                sha256: shasum,
                registry: Some(RegistryLock {
                    resolved_hostname: "registry.carina-rs.dev".into(),
                    api_base_url: "https://registry.carina-rs.dev/v1/providers/".into(),
                    discovery_sha256: sha256_bytes(
                        r#"{"providers.v1":"/v1/providers/"}"#.as_bytes(),
                    ),
                    sequence: RegistrySequence::Present(7),
                    sequence_anchor: RegistrySequenceAnchor::Established(7),
                    valid_until_present: true,
                    yanked_versions: YankedRegistryVersions::default(),
                    signature: signed_fixture_pin(),
                    transparency_log_present: false,
                }),
            })
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
        let RegistrySignatureProtection::Present(pin) = &registry.signature else {
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
        assert_eq!(registry.resolved_hostname, "registry.carina-rs.dev");
        assert_eq!(
            registry.api_base_url,
            "https://registry.carina-rs.dev/v1/providers/"
        );
        assert_eq!(registry.sequence, RegistrySequence::Present(7));
        assert_eq!(registry.signature, RegistrySignatureProtection::Absent);
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
        lock.upsert_registry(LockEntry {
            name: "aws".into(),
            source: "carina-rs/aws".into(),
            kind: LockEntryKind::RegistryRevision {
                revision: "main".into(),
                version: "0.0.0-main.1.aaa".into(),
            },
            sha256: old_shasum,
            registry: Some(RegistryLock {
                resolved_hostname: "registry.carina-rs.dev".into(),
                api_base_url: "https://registry.carina-rs.dev/v1/providers/".into(),
                discovery_sha256: sha256_bytes(r#"{"providers.v1":"/v1/providers/"}"#.as_bytes()),
                sequence: RegistrySequence::Present(7),
                sequence_anchor: RegistrySequenceAnchor::Established(7),
                valid_until_present: true,
                yanked_versions: YankedRegistryVersions::default(),
                signature: RegistrySignatureProtection::Absent,
                transparency_log_present: false,
            }),
        })
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
        lock.upsert_registry(LockEntry {
            name: "aws".into(),
            source: "carina-rs/aws".into(),
            kind: LockEntryKind::RegistryRevision {
                revision: "main".into(),
                version: "0.0.0-main.1.aaa".into(),
            },
            sha256: shasum.clone(),
            registry: Some(RegistryLock {
                resolved_hostname: "registry.carina-rs.dev".into(),
                api_base_url: "https://registry.carina-rs.dev/v1/providers/".into(),
                discovery_sha256: sha256_bytes(r#"{"providers.v1":"/v1/providers/"}"#.as_bytes()),
                sequence: RegistrySequence::Present(7),
                sequence_anchor: RegistrySequenceAnchor::Established(7),
                valid_until_present: true,
                yanked_versions: YankedRegistryVersions::default(),
                signature: RegistrySignatureProtection::Absent,
                transparency_log_present: false,
            }),
        })
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
            .upsert_registry(LockEntry {
                name: "aws".into(),
                source: "carina-rs/aws".into(),
                kind: LockEntryKind::Version {
                    version: "0.5.0".into(),
                    constraint: None,
                },
                sha256: shasum,
                registry: Some(RegistryLock {
                    resolved_hostname: "registry.carina-rs.dev".into(),
                    api_base_url: "https://registry.carina-rs.dev/v1/providers/".into(),
                    discovery_sha256: "old".into(),
                    sequence: RegistrySequence::Present(7),
                    sequence_anchor: RegistrySequenceAnchor::Established(7),
                    valid_until_present: true,
                    yanked_versions: YankedRegistryVersions::default(),
                    signature: RegistrySignatureProtection::Absent,
                    transparency_log_present: false,
                }),
            })
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
            .upsert_registry(LockEntry {
                name: "aws".into(),
                source: "carina-rs/aws".into(),
                kind: LockEntryKind::Version {
                    version: "0.5.0".into(),
                    constraint: None,
                },
                sha256: shasum,
                registry: Some(RegistryLock {
                    resolved_hostname: "registry.carina-rs.dev".into(),
                    api_base_url: "https://registry.carina-rs.dev/v1/providers/".into(),
                    discovery_sha256: "old".into(),
                    sequence: RegistrySequence::Present(7),
                    sequence_anchor: RegistrySequenceAnchor::Established(7),
                    valid_until_present: false,
                    yanked_versions: YankedRegistryVersions::default(),
                    signature: RegistrySignatureProtection::Absent,
                    transparency_log_present: false,
                }),
            })
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
            .upsert_registry(LockEntry {
                name: "aws".into(),
                source: "carina-rs/aws".into(),
                kind: LockEntryKind::Version {
                    version: "0.5.0".into(),
                    constraint: None,
                },
                sha256: shasum,
                registry: Some(RegistryLock {
                    resolved_hostname: "registry.carina-rs.dev".into(),
                    api_base_url: "https://registry.carina-rs.dev/v1/providers/".into(),
                    discovery_sha256: "old".into(),
                    sequence: RegistrySequence::Present(7),
                    sequence_anchor: RegistrySequenceAnchor::Established(7),
                    valid_until_present: true,
                    yanked_versions: YankedRegistryVersions::default(),
                    signature: RegistrySignatureProtection::Absent,
                    transparency_log_present: false,
                }),
            })
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
            .upsert_registry(LockEntry {
                name: "aws".into(),
                source: "carina-rs/aws".into(),
                kind: LockEntryKind::Version {
                    version: "0.5.0".into(),
                    constraint: None,
                },
                sha256: shasum,
                registry: Some(RegistryLock {
                    resolved_hostname: "registry.carina-rs.dev".into(),
                    api_base_url: "https://registry.carina-rs.dev/v1/providers/".into(),
                    discovery_sha256: "old".into(),
                    sequence: RegistrySequence::Present(7),
                    sequence_anchor: RegistrySequenceAnchor::Established(7),
                    valid_until_present: true,
                    yanked_versions: YankedRegistryVersions::default(),
                    signature: RegistrySignatureProtection::Absent,
                    transparency_log_present: false,
                }),
            })
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
            .upsert_registry(LockEntry {
                name: "aws".into(),
                source: "carina-rs/aws".into(),
                kind: LockEntryKind::Version {
                    version: "0.5.0".into(),
                    constraint: None,
                },
                sha256: shasum,
                registry: Some(RegistryLock {
                    resolved_hostname: "registry.carina-rs.dev".into(),
                    api_base_url: "https://registry.carina-rs.dev/v1/providers/".into(),
                    discovery_sha256: sha256_bytes(
                        r#"{"providers.v1":"/v1/providers/"}"#.as_bytes(),
                    ),
                    sequence: RegistrySequence::Present(7),
                    sequence_anchor: RegistrySequenceAnchor::Established(7),
                    valid_until_present: true,
                    yanked_versions: YankedRegistryVersions::default(),
                    signature: signed_fixture_pin(),
                    transparency_log_present: false,
                }),
            })
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
                "the resolved version of carina-rs/aws has no registry signature, but carina-providers.lock records this provider as signed"
            ),
            "{err}"
        );
        assert!(
            err.contains("downgrades from signed to unsigned versions are refused"),
            "{err}"
        );
        assert!(
            err.contains("deliberate downgrade to a pre-signing version"),
            "{err}"
        );
        assert!(err.contains("re-run carina init to re-pin"), "{err}");
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
            .upsert_registry(LockEntry {
                name: "aws".into(),
                source: "carina-rs/aws".into(),
                kind: LockEntryKind::Version {
                    version: "0.5.0".into(),
                    constraint: None,
                },
                sha256: old_shasum,
                registry: Some(RegistryLock {
                    resolved_hostname: "registry.carina-rs.dev".into(),
                    api_base_url: "https://registry.carina-rs.dev/v1/providers/".into(),
                    discovery_sha256: sha256_bytes(
                        r#"{"providers.v1":"/v1/providers/"}"#.as_bytes(),
                    ),
                    sequence: RegistrySequence::Present(7),
                    sequence_anchor: RegistrySequenceAnchor::Established(7),
                    valid_until_present: true,
                    yanked_versions: YankedRegistryVersions::default(),
                    signature: signed_fixture_pin(),
                    transparency_log_present: false,
                }),
            })
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
            .upsert_registry(LockEntry {
                name: "aws".into(),
                source: "carina-rs/aws".into(),
                kind: LockEntryKind::Version {
                    version: "0.5.0".into(),
                    constraint: None,
                },
                sha256: old_shasum,
                registry: Some(RegistryLock {
                    resolved_hostname: "registry.carina-rs.dev".into(),
                    api_base_url: "https://registry.carina-rs.dev/v1/providers/".into(),
                    discovery_sha256: sha256_bytes(
                        r#"{"providers.v1":"/v1/providers/"}"#.as_bytes(),
                    ),
                    sequence: RegistrySequence::Present(7),
                    sequence_anchor: RegistrySequenceAnchor::Established(7),
                    valid_until_present: true,
                    yanked_versions: YankedRegistryVersions::default(),
                    signature: signed_fixture_pin(),
                    transparency_log_present: false,
                }),
            })
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
            .upsert_registry(LockEntry {
                name: "aws".into(),
                source: "carina-rs/aws".into(),
                kind: LockEntryKind::Version {
                    version: "0.5.0".into(),
                    constraint: None,
                },
                sha256: shasum,
                registry: Some(RegistryLock {
                    resolved_hostname: "registry.carina-rs.dev".into(),
                    api_base_url: "https://registry.carina-rs.dev/v1/providers/".into(),
                    discovery_sha256: sha256_bytes(
                        r#"{"providers.v1":"/v1/providers/"}"#.as_bytes(),
                    ),
                    sequence: RegistrySequence::Present(7),
                    sequence_anchor: RegistrySequenceAnchor::Established(7),
                    valid_until_present: true,
                    yanked_versions: YankedRegistryVersions::default(),
                    signature: signature_pin("identity", "issuer"),
                    transparency_log_present: true,
                }),
            })
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
