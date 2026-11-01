use std::collections::BTreeSet;
use std::error::Error;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use carina_core::hint::ProjectCommand;
use carina_core::provider_source::display_untrusted;
use carina_provider_protocol::{HostPattern, HostPatternError};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use wasmparser::{Parser, Payload};

use crate::{ProviderLoadSource, VerifiedLoad};

const EGRESS_SECTION_NAME: &str = "carina:egress";
const EGRESS_SECTION_VERSION: u64 = 1;
/// Shared HTTP-world classification used for both compiled component imports
/// and the exact bytes examined before instantiation.
pub fn is_wasi_http_import(name: &str) -> bool {
    name.starts_with("wasi:http/")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanonicalSource(CanonicalSourceKind);

#[derive(Debug, Clone, PartialEq, Eq)]
enum CanonicalSourceKind {
    File(PathBuf),
    Remote(carina_core::provider_source::RemoteSourceKey),
}

impl CanonicalSource {
    pub(crate) fn from_load(load: &VerifiedLoad) -> Self {
        let kind = match load.identity().source() {
            ProviderLoadSource::LocalFile {
                path: _,
                canonical_source,
            } => CanonicalSourceKind::File(canonical_source.clone()),
            ProviderLoadSource::Remote { source } => CanonicalSourceKind::Remote(source.clone()),
        };
        Self(kind)
    }

    pub fn is_file(&self) -> bool {
        match self.0 {
            CanonicalSourceKind::File(_) => true,
            CanonicalSourceKind::Remote(_) => false,
        }
    }

    pub(crate) fn from_lock_source(source: &str, project_dir: &Path) -> Option<Self> {
        match source
            .parse::<carina_core::provider_source::ProviderSource>()
            .ok()?
        {
            carina_core::provider_source::ProviderSource::File(source) => {
                canonical_project_relative_path(project_dir, Path::new(source.path()))
                    .ok()
                    .map(|path| Self(CanonicalSourceKind::File(path)))
            }
            carina_core::provider_source::ProviderSource::Remote(source) => {
                Some(Self(CanonicalSourceKind::Remote(source.source_key())))
            }
        }
    }
}

impl fmt::Display for CanonicalSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.0 {
            CanonicalSourceKind::File(path) => write!(formatter, "file://{}", path.display()),
            CanonicalSourceKind::Remote(source) => source.fmt(formatter),
        }
    }
}

pub(crate) fn canonical_project_relative_path(
    project_dir: &Path,
    source_path: &Path,
) -> io::Result<PathBuf> {
    let project_dir = normalize_path(&std::path::absolute(project_dir)?);
    let source_path = normalize_path(&std::path::absolute(source_path)?);
    relative_path(&project_dir, &source_path).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "file provider and project directory are on different filesystem roots",
        )
    })
}

fn relative_path(base: &Path, target: &Path) -> Option<PathBuf> {
    let base = base.components().collect::<Vec<_>>();
    let target = target.components().collect::<Vec<_>>();
    let common = base
        .iter()
        .zip(&target)
        .take_while(|(left, right)| left == right)
        .count();
    if common == 0 {
        return None;
    }

    let mut relative = PathBuf::new();
    for component in &base[common..] {
        if matches!(component, std::path::Component::Normal(_)) {
            relative.push("..");
        }
    }
    for component in &target[common..] {
        relative.push(component.as_os_str());
    }
    if relative.as_os_str().is_empty() {
        relative.push(".");
    }
    Some(relative)
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => match normalized.components().next_back() {
                Some(std::path::Component::Normal(_)) => {
                    normalized.pop();
                }
                Some(std::path::Component::ParentDir) | None => normalized.push(".."),
                Some(std::path::Component::RootDir | std::path::Component::Prefix(_)) => {}
                Some(std::path::Component::CurDir) => unreachable!(),
            },
            std::path::Component::Normal(component) => normalized.push(component),
            std::path::Component::RootDir | std::path::Component::Prefix(_) => {
                normalized.push(component.as_os_str());
            }
        }
    }
    normalized
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sha256(String);

impl Sha256 {
    pub(crate) fn from_lock(value: String) -> Self {
        Self(value)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedEgressDeclaration {
    source: CanonicalSource,
    sha256: Sha256,
    patterns: BTreeSet<HostPattern>,
}

impl VerifiedEgressDeclaration {
    pub fn read(load: &VerifiedLoad) -> Result<Self, EgressSectionError> {
        let mut sections = Vec::new();
        let mut imports_http = false;
        let mut depth = 0usize;

        for payload in Parser::new(0).parse_all(load.bytes().as_bytes()) {
            let payload = payload.map_err(EgressSectionError::InvalidWasm)?;
            match payload {
                Payload::ModuleSection { .. } | Payload::ComponentSection { .. } => {
                    depth += 1;
                }
                Payload::End(_) if depth > 0 => {
                    depth -= 1;
                }
                Payload::ComponentImportSection(imports) if depth == 0 => {
                    for import in imports {
                        let import = import.map_err(EgressSectionError::InvalidWasm)?;
                        imports_http |= is_wasi_http_import(import.name.0);
                    }
                }
                Payload::CustomSection(section) if section.name() == EGRESS_SECTION_NAME => {
                    sections.push(section.data());
                }
                _ => {}
            }
        }

        let patterns = match sections.as_slice() {
            [] if imports_http => return Err(EgressSectionError::MissingForHttp),
            [] => BTreeSet::new(),
            [payload] => parse_declaration(payload)?,
            [_, _, ..] => return Err(EgressSectionError::Duplicate),
        };

        Ok(Self {
            source: CanonicalSource::from_load(load),
            sha256: Sha256(load.artifact().sha256().to_string()),
            patterns,
        })
    }

    pub fn patterns(&self) -> &BTreeSet<HostPattern> {
        &self.patterns
    }

    pub fn source(&self) -> &CanonicalSource {
        &self.source
    }

    pub fn sha256(&self) -> &Sha256 {
        &self.sha256
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawEgressDeclaration {
    version: u64,
    hosts: Vec<String>,
}

fn parse_declaration(payload: &[u8]) -> Result<BTreeSet<HostPattern>, EgressSectionError> {
    let raw: RawEgressDeclaration =
        serde_json::from_slice(payload).map_err(EgressSectionError::MalformedJson)?;
    if raw.version != EGRESS_SECTION_VERSION {
        return Err(EgressSectionError::UnknownVersion(raw.version));
    }
    raw.hosts
        .into_iter()
        .map(|pattern| {
            HostPattern::parse(&pattern)
                .map_err(|source| EgressSectionError::InvalidPattern { pattern, source })
        })
        .collect()
}

#[derive(Debug)]
pub enum EgressSectionError {
    MissingForHttp,
    Duplicate,
    UnknownVersion(u64),
    MalformedJson(serde_json::Error),
    InvalidPattern {
        pattern: String,
        source: HostPatternError,
    },
    InvalidWasm(wasmparser::BinaryReaderError),
}

impl fmt::Display for EgressSectionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingForHttp => write!(
                formatter,
                "HTTP provider is missing the carina:egress declaration; rebuild it with a carina-plugin-sdk that embeds the carina:egress section (carina#4021 or later)"
            ),
            Self::Duplicate => {
                formatter.write_str("provider contains duplicate carina:egress sections")
            }
            Self::UnknownVersion(version) => {
                write!(formatter, "unknown egress declaration version {version}")
            }
            Self::MalformedJson(source) => {
                write!(formatter, "malformed egress declaration JSON: {source}")
            }
            Self::InvalidPattern { pattern, source } => {
                write!(
                    formatter,
                    "invalid egress host pattern '{}': {source}",
                    display_untrusted(pattern)
                )
            }
            Self::InvalidWasm(source) => {
                write!(formatter, "failed to read provider component: {source}")
            }
        }
    }
}

impl Error for EgressSectionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        None
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum LockedConsentKind {
    Explicit,
    CarinaRsSigned,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EgressLockRecord {
    consent: LockedConsentKind,
    #[serde(
        serialize_with = "serialize_host_patterns",
        deserialize_with = "deserialize_host_patterns"
    )]
    hosts: LockedHostPatterns,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum LockedHostPatterns {
    Valid(BTreeSet<HostPattern>),
    Invalid {
        raw: Vec<String>,
        pattern: String,
        detail: String,
    },
}

impl EgressLockRecord {
    pub(crate) fn explicit(hosts: BTreeSet<HostPattern>) -> Self {
        Self {
            consent: LockedConsentKind::Explicit,
            hosts: LockedHostPatterns::Valid(hosts),
        }
    }

    pub(crate) fn carina_rs_signed(hosts: BTreeSet<HostPattern>) -> Self {
        Self {
            consent: LockedConsentKind::CarinaRsSigned,
            hosts: LockedHostPatterns::Valid(hosts),
        }
    }

    pub fn hosts(&self) -> Option<&BTreeSet<HostPattern>> {
        match &self.hosts {
            LockedHostPatterns::Valid(hosts) => Some(hosts),
            LockedHostPatterns::Invalid { .. } => None,
        }
    }

    fn invalid_host(&self) -> Option<(&str, &str)> {
        match &self.hosts {
            LockedHostPatterns::Valid(_) => None,
            LockedHostPatterns::Invalid {
                raw: _,
                pattern,
                detail,
            } => Some((pattern, detail)),
        }
    }
}

fn serialize_host_patterns<S>(
    patterns: &LockedHostPatterns,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    let mut patterns = match patterns {
        LockedHostPatterns::Valid(patterns) => {
            patterns.iter().map(ToString::to_string).collect::<Vec<_>>()
        }
        LockedHostPatterns::Invalid {
            raw,
            pattern: _,
            detail: _,
        } => raw.clone(),
    };
    patterns.sort();
    patterns.serialize(serializer)
}

fn deserialize_host_patterns<'de, D>(deserializer: D) -> Result<LockedHostPatterns, D::Error>
where
    D: Deserializer<'de>,
{
    let raw = Vec::<String>::deserialize(deserializer)?;
    let mut parsed = BTreeSet::new();
    for pattern in &raw {
        match HostPattern::parse(pattern) {
            Ok(pattern) => {
                parsed.insert(pattern);
            }
            Err(error) => {
                let invalid_pattern = pattern.clone();
                return Ok(LockedHostPatterns::Invalid {
                    raw,
                    pattern: invalid_pattern,
                    detail: error.to_string(),
                });
            }
        }
    }
    Ok(LockedHostPatterns::Valid(parsed))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockedEgress {
    source: CanonicalSource,
    sha256: Sha256,
    record: EgressLockRecord,
    project_dir: PathBuf,
}

impl LockedEgress {
    pub(crate) fn new(
        source: CanonicalSource,
        sha256: Sha256,
        record: EgressLockRecord,
        project_dir: &Path,
    ) -> Self {
        Self {
            source,
            sha256,
            record,
            project_dir: project_dir.to_path_buf(),
        }
    }

    pub(crate) fn explicit_consent(&self) -> Option<ExplicitConsent> {
        match self.record.consent {
            LockedConsentKind::Explicit => ExplicitConsent::from_lock(self),
            LockedConsentKind::CarinaRsSigned => None,
        }
    }
}

/// Proof that a remote artifact was loaded through the resolver's
/// carina-rs signature verification path.
///
/// ```compile_fail,E0451
/// use carina_provider_resolver::{CarinaRsSigned, CanonicalSource, Sha256};
/// # let source: CanonicalSource = panic!();
/// # let sha256: Sha256 = panic!();
/// let _ = CarinaRsSigned { source, sha256 };
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CarinaRsSigned {
    source: CanonicalSource,
    sha256: Sha256,
}

impl CarinaRsSigned {
    pub(crate) fn for_load(load: &VerifiedLoad) -> Option<Self> {
        load.identity().trust().is_carina_rs_signed().then(|| Self {
            source: CanonicalSource::from_load(load),
            sha256: Sha256(load.artifact().sha256().to_string()),
        })
    }
}

/// A grant for one canonical source and one exact declaration.
///
/// ```compile_fail,E0451
/// use std::collections::BTreeSet;
/// use carina_provider_resolver::{CanonicalSource, ExplicitConsent, HostPattern};
/// # let source: CanonicalSource = panic!();
/// let _ = ExplicitConsent { source, patterns: BTreeSet::<HostPattern>::new() };
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExplicitConsent {
    source: CanonicalSource,
    patterns: BTreeSet<HostPattern>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ConsentGiven(ConsentGivenKind);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConsentGivenKind {
    Prompt,
    AcceptedSource,
}

impl ExplicitConsent {
    pub(crate) fn granted(declaration: &VerifiedEgressDeclaration, _given: ConsentGiven) -> Self {
        Self {
            source: declaration.source.clone(),
            patterns: declaration.patterns.clone(),
        }
    }

    pub(crate) fn carry_over(
        previous: ExplicitConsent,
        declaration: &VerifiedEgressDeclaration,
    ) -> Result<Self, CarryOverRefused> {
        if previous.source != declaration.source {
            return Err(CarryOverRefused::SourceChanged);
        }
        if previous.patterns != declaration.patterns {
            return Err(CarryOverRefused::DeclarationChanged);
        }
        Ok(Self {
            source: declaration.source.clone(),
            patterns: declaration.patterns.clone(),
        })
    }

    pub(crate) fn from_lock(locked: &LockedEgress) -> Option<Self> {
        Some(Self {
            source: locked.source.clone(),
            patterns: locked.record.hosts()?.clone(),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CarryOverRefused {
    SourceChanged,
    DeclarationChanged,
}

impl fmt::Display for CarryOverRefused {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SourceChanged => formatter.write_str("the provider source changed"),
            Self::DeclarationChanged => formatter.write_str("the egress declaration changed"),
        }
    }
}

impl Error for CarryOverRefused {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EgressConsent {
    CarinaRsSigned(CarinaRsSigned),
    Explicit(ExplicitConsent),
}

/// The only hostname permission accepted by the WASM host.
///
/// ```compile_fail,E0451
/// use std::collections::BTreeSet;
/// use carina_provider_resolver::{ConsentedEgress, HostPattern};
/// let _ = ConsentedEgress { patterns: BTreeSet::<HostPattern>::new() };
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsentedEgress {
    patterns: BTreeSet<HostPattern>,
}

impl ConsentedEgress {
    pub fn from_lock(
        entry: Option<&LockedEgress>,
        declaration: VerifiedEgressDeclaration,
        signed: Option<CarinaRsSigned>,
    ) -> Result<Self, EgressConsentError> {
        let Some(entry) = entry else {
            return Self::nothing_declared(declaration);
        };
        if entry.source != declaration.source {
            return Err(EgressConsentError::SourceMismatch {
                locked: entry.source.to_string(),
                loaded: declaration.source.to_string(),
            });
        }
        let Some(locked_patterns) = entry.record.hosts() else {
            let (pattern, detail) = entry
                .record
                .invalid_host()
                .expect("an invalid locked host state carries its parse failure");
            return Err(EgressConsentError::InvalidLockedPattern {
                source: entry.source.to_string(),
                pattern: pattern.to_string(),
                detail: detail.to_string(),
                project_dir: entry.project_dir.clone(),
            });
        };
        if locked_patterns != &declaration.patterns {
            return Err(EgressConsentError::DeclarationMismatch);
        }
        if !entry.source.is_file() && entry.sha256 != declaration.sha256 {
            return Err(EgressConsentError::Sha256Mismatch);
        }

        let consent = match entry.record.consent {
            LockedConsentKind::Explicit => EgressConsent::Explicit(
                ExplicitConsent::from_lock(entry).expect("the locked hosts were validated above"),
            ),
            LockedConsentKind::CarinaRsSigned => EgressConsent::CarinaRsSigned(
                signed.ok_or(EgressConsentError::MissingCarinaRsSignedWitness)?,
            ),
        };
        Self::from_consent(declaration, consent)
    }

    pub fn nothing_declared(
        declaration: VerifiedEgressDeclaration,
    ) -> Result<Self, EgressConsentError> {
        if !declaration.patterns.is_empty() {
            return Err(EgressConsentError::MissingConsent);
        }
        Ok(Self {
            patterns: declaration.patterns,
        })
    }

    pub(crate) fn from_consent(
        declaration: VerifiedEgressDeclaration,
        consent: EgressConsent,
    ) -> Result<Self, EgressConsentError> {
        match consent {
            EgressConsent::CarinaRsSigned(signed) => {
                if signed.source != declaration.source || signed.sha256 != declaration.sha256 {
                    return Err(EgressConsentError::SignedWitnessMismatch);
                }
            }
            EgressConsent::Explicit(explicit) => {
                if explicit.source != declaration.source {
                    return Err(EgressConsentError::ExplicitSourceMismatch);
                }
                if explicit.patterns != declaration.patterns {
                    return Err(EgressConsentError::DeclarationMismatch);
                }
            }
        }
        Ok(Self {
            patterns: declaration.patterns,
        })
    }

    pub fn allows(&self, host: &str) -> bool {
        self.patterns.iter().any(|pattern| pattern.matches(host))
    }

    pub fn patterns(&self) -> &BTreeSet<HostPattern> {
        &self.patterns
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EgressConsentError {
    SourceMismatch {
        locked: String,
        loaded: String,
    },
    ExplicitSourceMismatch,
    DeclarationMismatch,
    Sha256Mismatch,
    MissingCarinaRsSignedWitness,
    SignedWitnessMismatch,
    MissingConsent,
    MissingLockEntry,
    InvalidLockedPattern {
        source: String,
        pattern: String,
        detail: String,
        project_dir: PathBuf,
    },
}

impl fmt::Display for EgressConsentError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SourceMismatch { locked, loaded } => write!(
                formatter,
                "locked egress source {locked} does not match loaded provider source {loaded}"
            ),
            Self::ExplicitSourceMismatch => formatter.write_str(
                "explicit egress consent belongs to a different provider source",
            ),
            Self::DeclarationMismatch => formatter.write_str(
                "the provider's egress declaration differs from the lock",
            ),
            Self::Sha256Mismatch => formatter.write_str(
                "the loaded provider digest differs from the egress consent record",
            ),
            Self::MissingCarinaRsSignedWitness => formatter.write_str(
                "the lock claims carina-rs-signed egress consent, but this load has no carina-rs signature witness",
            ),
            Self::SignedWitnessMismatch => formatter.write_str(
                "the carina-rs signature witness belongs to different provider bytes",
            ),
            Self::MissingConsent => formatter.write_str(
                "the provider declares egress hosts but has no consented egress record",
            ),
            Self::MissingLockEntry => {
                formatter.write_str("the provider declaration has no matching lock entry")
            }
            Self::InvalidLockedPattern {
                source,
                pattern,
                detail,
                project_dir,
            } => write!(
                formatter,
                "locked egress consent for provider source {} contains invalid host pattern '{}': {detail}; run `{}` to review and repair it",
                display_untrusted(source),
                display_untrusted(pattern),
                ProjectCommand::new("init", project_dir)
            ),
        }
    }
}

impl Error for EgressConsentError {}

/// Canonical provider sources explicitly accepted for one `carina init` run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AcceptedEgressSources(BTreeSet<String>);

impl AcceptedEgressSources {
    pub fn parse_os(
        cli_values: &[String],
        env_value: Option<&OsStr>,
        project_dir: &Path,
    ) -> Result<Self, AcceptedEgressSourceError> {
        let mut sources = BTreeSet::new();
        for value in cli_values {
            sources.insert(normalize_accepted_source(value, project_dir)?);
        }
        if let Some(env_value) = env_value.filter(|value| !value.is_empty()) {
            let value = env_value.to_str().ok_or_else(|| {
                AcceptedEgressSourceError::NonUtf8Environment {
                    value: env_value.to_os_string(),
                }
            })?;
            for value in value
                .split(',')
                .map(str::trim)
                .filter(|value| !value.is_empty())
            {
                sources.insert(normalize_accepted_source(value, project_dir)?);
            }
        }
        Ok(Self(sources))
    }

    pub(crate) fn accepts(&self, source: &CanonicalSource) -> Option<ConsentGiven> {
        self.0
            .contains(&source.to_string())
            .then_some(ConsentGiven(ConsentGivenKind::AcceptedSource))
    }
}

pub(crate) fn prompt_for_explicit_consent(
    source: &CanonicalSource,
    input: &mut impl io::BufRead,
    output: &mut impl io::Write,
) -> io::Result<Option<ConsentGiven>> {
    write!(
        output,
        "Allow these egress hosts for provider source {source}? [y/N] "
    )?;
    output.flush()?;
    let mut response = String::new();
    input.read_line(&mut response)?;
    Ok(
        matches!(response.trim().to_ascii_lowercase().as_str(), "y" | "yes")
            .then_some(ConsentGiven(ConsentGivenKind::Prompt)),
    )
}

fn normalize_accepted_source(
    value: &str,
    project_dir: &Path,
) -> Result<String, AcceptedEgressSourceError> {
    let file_path = value.strip_prefix("file://");
    let path_value = file_path.unwrap_or(value);
    let path = Path::new(path_value);
    if file_path.is_some() || path.is_absolute() {
        let resolved = if file_path.is_some() && path.is_relative() {
            project_dir.join(path)
        } else {
            path.to_path_buf()
        };
        let canonical =
            canonical_project_relative_path(project_dir, &resolved).map_err(|source| {
                AcceptedEgressSourceError::Canonicalize {
                    value: value.to_string(),
                    path: path.to_path_buf(),
                    source,
                }
            })?;
        return Ok(format!("file://{}", canonical.display()));
    }

    let parsed = value
        .parse::<carina_core::provider_source::ProviderSource>()
        .map_err(|source| AcceptedEgressSourceError::InvalidSource {
            value: value.to_string(),
            source,
        })?;
    match parsed {
        carina_core::provider_source::ProviderSource::Remote(source) => {
            Ok(source.source_key().to_string())
        }
        carina_core::provider_source::ProviderSource::File(_) => {
            unreachable!("file:// sources are handled before ProviderSource parsing")
        }
    }
}

#[derive(Debug)]
pub enum AcceptedEgressSourceError {
    InvalidSource {
        value: String,
        source: carina_core::provider_source::ProviderSourceParseError,
    },
    Canonicalize {
        value: String,
        path: PathBuf,
        source: io::Error,
    },
    NonUtf8Environment {
        value: OsString,
    },
}

impl fmt::Display for AcceptedEgressSourceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSource { value, source } => {
                write!(
                    formatter,
                    "invalid --accept-egress source \"{}\": ",
                    display_untrusted(value)
                )?;
                match source {
                    carina_core::provider_source::ProviderSourceParseError::InvalidFormat {
                        input: _,
                    } => formatter.write_str(
                        "expected file://<path>, github.com/{owner}/{repo}, or [hostname/]namespace/name",
                    ),
                    carina_core::provider_source::ProviderSourceParseError::FilePathContainsControl {
                        input: _,
                    } => formatter
                        .write_str("file:// paths must not contain control characters"),
                }
            }
            Self::Canonicalize {
                value,
                path,
                source,
            } => write!(
                formatter,
                "invalid --accept-egress source {value:?}: failed to canonicalize {}: {source}",
                path.display()
            ),
            Self::NonUtf8Environment { value } => write!(
                formatter,
                "invalid CARINA_ACCEPT_EGRESS entry {value:?}: source must be valid UTF-8"
            ),
        }
    }
}

impl Error for AcceptedEgressSourceError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        None
    }
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;
    use std::ffi::OsString;
    use std::io::Cursor;
    use std::path::{Path, PathBuf};

    use carina_core::UserDirs;
    use sha2::{Digest, Sha256 as Sha256Hasher};
    use wasm_encoder::{
        Component, ComponentImportSection, ComponentTypeRef, CustomSection, Module, ModuleSection,
    };

    use super::{
        AcceptedEgressSources, CanonicalSource, CanonicalSourceKind, ConsentedEgress,
        EgressLockRecord, EgressSectionError, ExplicitConsent, LockedEgress,
        VerifiedEgressDeclaration,
    };
    use crate::user_trust::load_user_verified_identity;
    use crate::verified_artifact::record_test_github_origin_download;
    use crate::{
        ProviderLoadIdentity, ProviderLoadSource, ProviderTrust, ProviderTrustClass, VerifiedLoad,
    };

    const VALID_DECLARATION: &[u8] = br#"{"version":1,"hosts":["api.example.com"]}"#;

    fn custom(data: &'static [u8]) -> CustomSection<'static> {
        CustomSection {
            name: Cow::Borrowed("carina:egress"),
            data: Cow::Borrowed(data),
        }
    }

    fn component_with_sections(sections: &[&'static [u8]]) -> Vec<u8> {
        let mut component = Component::new();
        for data in sections {
            component.section(&custom(data));
        }
        component.finish()
    }

    fn load(bytes: &[u8]) -> VerifiedLoad {
        load_named(bytes, "mock", ProviderTrustClass::Untrusted)
    }

    fn load_named(bytes: &[u8], name: &str, class: ProviderTrustClass) -> VerifiedLoad {
        VerifiedLoad::for_test(bytes, ProviderLoadIdentity::for_test(name, class))
    }

    fn classified_signed_remote_load(bytes: &[u8]) -> VerifiedLoad {
        let trust_root = tempfile::tempdir().unwrap();
        let user_dirs = UserDirs::under(trust_root.path());
        let parsed = "github.com/carina-rs/carina-provider-aws"
            .parse::<carina_core::provider_source::ProviderSource>()
            .unwrap();
        let source = match parsed {
            carina_core::provider_source::ProviderSource::Remote(source) => source.source_key(),
            carina_core::provider_source::ProviderSource::File(_) => unreachable!(),
        };
        let sha256 = format!("{:x}", Sha256Hasher::digest(bytes));
        record_test_github_origin_download(&user_dirs, &source, &sha256, None).unwrap();
        let verified_identity = load_user_verified_identity(&user_dirs, &sha256, &source)
            .unwrap()
            .unwrap();
        let trust = ProviderTrust::from_verified_remote(&verified_identity).unwrap();
        let identity =
            ProviderLoadIdentity::new("aws".into(), trust, ProviderLoadSource::Remote { source })
                .unwrap();
        VerifiedLoad::for_test(bytes, identity)
    }

    fn component_with_declaration_and_build_id(declaration: &'static [u8], id: u8) -> Vec<u8> {
        let mut component = Component::new();
        component.section(&custom(declaration));
        component.section(&CustomSection {
            name: Cow::Borrowed("build:id"),
            data: Cow::Owned(vec![id]),
        });
        component.finish()
    }

    #[test]
    fn absent_section_without_http_import_is_empty() {
        let declaration =
            VerifiedEgressDeclaration::read(&load(&Component::new().finish())).unwrap();
        assert!(declaration.patterns().is_empty());
    }

    #[test]
    fn invalid_section_pattern_does_not_render_control_characters_raw() {
        let pattern = "bad\u{1b}\u{7}.example.com".to_string();
        let source = carina_provider_protocol::HostPattern::parse(&pattern).unwrap_err();
        let rendered = EgressSectionError::InvalidPattern { pattern, source }.to_string();

        assert!(!rendered.contains('\u{1b}'), "{rendered:?}");
        assert!(!rendered.contains('\u{7}'), "{rendered:?}");
        assert_eq!(rendered.matches(r"\u{1b}").count(), 1, "{rendered:?}");
        assert_eq!(rendered.matches(r"\u{7}").count(), 1, "{rendered:?}");
    }

    #[test]
    fn empty_declaration_builds_deny_all_permission_without_a_lock_record() {
        let declaration =
            VerifiedEgressDeclaration::read(&load(&Component::new().finish())).unwrap();

        let consented = ConsentedEgress::from_lock(None, declaration, None).unwrap();

        assert!(consented.patterns().is_empty());
        assert!(!consented.allows("api.example.com"));
    }

    #[test]
    fn nonempty_declaration_without_a_lock_record_is_refused() {
        let declaration =
            VerifiedEgressDeclaration::read(&load(&component_with_sections(&[VALID_DECLARATION])))
                .unwrap();

        assert!(matches!(
            ConsentedEgress::from_lock(None, declaration, None),
            Err(super::EgressConsentError::MissingConsent)
        ));
    }

    #[test]
    fn absent_section_with_http_import_is_an_error() {
        let mut imports = ComponentImportSection::new();
        imports.import(
            "wasi:http/outgoing-handler@0.2.6",
            ComponentTypeRef::Instance(0),
        );
        let mut component = Component::new();
        component.section(&imports);

        let error = VerifiedEgressDeclaration::read(&load(&component.finish())).unwrap_err();
        assert!(matches!(&error, EgressSectionError::MissingForHttp));
        assert!(
            error
                .to_string()
                .contains("carina:egress section (carina#4021 or later)")
        );
    }

    #[test]
    fn duplicate_section_is_an_error() {
        assert!(matches!(
            VerifiedEgressDeclaration::read(&load(&component_with_sections(&[
                VALID_DECLARATION,
                VALID_DECLARATION,
            ]))),
            Err(EgressSectionError::Duplicate)
        ));
    }

    #[test]
    fn malformed_unknown_and_extra_fields_are_errors() {
        let cases = [
            (br#"{"version":1,"hosts":[}"#.as_slice(), "malformed"),
            (br#"{"version":2,"hosts":[]}"#.as_slice(), "version"),
            (
                br#"{"version":1,"hosts":[],"unexpected":true}"#.as_slice(),
                "unknown field",
            ),
        ];
        for (data, expected) in cases {
            let error = VerifiedEgressDeclaration::read(&load(&component_with_sections(&[data])))
                .unwrap_err();
            let rendered = error.to_string();
            assert!(rendered.contains(expected), "{rendered}");
        }
    }

    #[test]
    fn invalid_pattern_in_section_is_an_error() {
        assert!(matches!(
            VerifiedEgressDeclaration::read(&load(&component_with_sections(&[
                br#"{"version":1,"hosts":["*.com"]}"#,
            ]))),
            Err(EgressSectionError::InvalidPattern { .. })
        ));
    }

    #[test]
    fn section_inside_nested_core_module_is_read() {
        let mut module = Module::new();
        module.section(&custom(VALID_DECLARATION));
        let mut component = Component::new();
        component.section(&ModuleSection(&module));

        let declaration = VerifiedEgressDeclaration::read(&load(&component.finish())).unwrap();
        assert_eq!(
            declaration.patterns().iter().next().unwrap().to_string(),
            "api.example.com"
        );
    }

    #[test]
    fn explicit_consent_carries_over_only_for_the_same_source_and_patterns() {
        let first = VerifiedEgressDeclaration::read(&load_named(
            &component_with_declaration_and_build_id(VALID_DECLARATION, 1),
            "first",
            ProviderTrustClass::Untrusted,
        ))
        .unwrap();
        let same_source = VerifiedEgressDeclaration::read(&load_named(
            &component_with_declaration_and_build_id(VALID_DECLARATION, 2),
            "first",
            ProviderTrustClass::Untrusted,
        ))
        .unwrap();
        let other_source = VerifiedEgressDeclaration::read(&load_named(
            &component_with_declaration_and_build_id(VALID_DECLARATION, 2),
            "second",
            ProviderTrustClass::Untrusted,
        ))
        .unwrap();

        let mut input = Cursor::new(b"yes\n");
        let mut output = Vec::new();
        let given = super::prompt_for_explicit_consent(first.source(), &mut input, &mut output)
            .unwrap()
            .unwrap();
        let previous = ExplicitConsent::granted(&first, given);
        assert!(ExplicitConsent::carry_over(previous.clone(), &same_source).is_ok());
        assert!(ExplicitConsent::carry_over(previous, &other_source).is_err());
    }

    #[test]
    fn lock_restore_refuses_a_declaration_from_a_different_source() {
        let first = VerifiedEgressDeclaration::read(&load_named(
            &component_with_sections(&[VALID_DECLARATION]),
            "first",
            ProviderTrustClass::Untrusted,
        ))
        .unwrap();
        let other = VerifiedEgressDeclaration::read(&load_named(
            &component_with_sections(&[VALID_DECLARATION]),
            "other",
            ProviderTrustClass::Untrusted,
        ))
        .unwrap();
        let locked = LockedEgress::new(
            first.source().clone(),
            first.sha256().clone(),
            EgressLockRecord::explicit(first.patterns().clone()),
            Path::new("."),
        );

        assert!(matches!(
            ConsentedEgress::from_lock(Some(&locked), other, None),
            Err(super::EgressConsentError::SourceMismatch { .. })
        ));
    }

    #[test]
    fn trusted_local_classification_cannot_back_a_signed_lock_record() {
        let load = load_named(
            &component_with_sections(&[VALID_DECLARATION]),
            "aws",
            ProviderTrustClass::Aws,
        );
        let signed = load.carina_rs_signed();
        let declaration = VerifiedEgressDeclaration::read(&load).unwrap();
        let locked = LockedEgress::new(
            declaration.source().clone(),
            declaration.sha256().clone(),
            EgressLockRecord::carina_rs_signed(declaration.patterns().clone()),
            Path::new("."),
        );

        assert!(signed.is_none());
        assert!(ConsentedEgress::from_lock(Some(&locked), declaration, signed).is_err());
    }

    #[test]
    fn classified_verified_remote_load_backs_a_signed_lock_record() {
        let bytes = component_with_sections(&[VALID_DECLARATION]);
        let load = classified_signed_remote_load(&bytes);
        let signed = load
            .carina_rs_signed()
            .expect("the classified verified remote load mints the witness");
        let declaration = VerifiedEgressDeclaration::read(&load).unwrap();
        let locked = LockedEgress::new(
            declaration.source().clone(),
            declaration.sha256().clone(),
            EgressLockRecord::carina_rs_signed(declaration.patterns().clone()),
            Path::new("."),
        );

        let consented =
            ConsentedEgress::from_lock(Some(&locked), declaration, Some(signed)).unwrap();
        assert!(consented.allows("api.example.com"));
    }

    #[test]
    fn file_rebuild_with_unchanged_declaration_ignores_locked_sha256() {
        let first = VerifiedEgressDeclaration::read(&load(
            &component_with_declaration_and_build_id(VALID_DECLARATION, 1),
        ))
        .unwrap();
        let rebuilt = VerifiedEgressDeclaration::read(&load(
            &component_with_declaration_and_build_id(VALID_DECLARATION, 2),
        ))
        .unwrap();
        assert_ne!(first.sha256(), rebuilt.sha256());
        let locked = LockedEgress::new(
            first.source().clone(),
            first.sha256().clone(),
            EgressLockRecord::explicit(first.patterns().clone()),
            Path::new("."),
        );

        let consented = ConsentedEgress::from_lock(Some(&locked), rebuilt, None).unwrap();
        assert!(consented.allows("api.example.com"));
    }

    #[test]
    fn file_rebuild_with_changed_declaration_is_refused() {
        let first =
            VerifiedEgressDeclaration::read(&load(&component_with_sections(&[VALID_DECLARATION])))
                .unwrap();
        let changed = VerifiedEgressDeclaration::read(&load(&component_with_sections(&[
            br#"{"version":1,"hosts":["other.example.com"]}"#,
        ])))
        .unwrap();
        let locked = LockedEgress::new(
            first.source().clone(),
            first.sha256().clone(),
            EgressLockRecord::explicit(first.patterns().clone()),
            Path::new("."),
        );

        assert!(ConsentedEgress::from_lock(Some(&locked), changed, None).is_err());
    }

    #[test]
    fn accepted_egress_sources_match_canonical_sources_not_configured_names() {
        let accepted = AcceptedEgressSources::parse_os(
            &["Registry.Carina-RS.dev/example/cloudflare".into()],
            None,
            Path::new("."),
        )
        .unwrap();
        let parsed = "example/cloudflare"
            .parse::<carina_core::provider_source::ProviderSource>()
            .unwrap();
        let source_key = match parsed {
            carina_core::provider_source::ProviderSource::Remote(source) => source.source_key(),
            carina_core::provider_source::ProviderSource::File(_) => {
                panic!("remote fixture parsed as a file source")
            }
        };
        let source = CanonicalSource(CanonicalSourceKind::Remote(source_key));

        assert!(accepted.accepts(&source).is_some());
        assert!(
            !AcceptedEgressSources::parse_os(&["configured_name".into()], None, Path::new("."))
                .is_ok_and(|accepted| accepted.accepts(&source).is_some())
        );
    }

    #[test]
    fn accepted_file_sources_use_a_trimmed_comma_separated_environment_list() {
        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join("first.wasm");
        let second = directory.path().join("second.wasm");
        std::fs::write(&first, b"first").unwrap();
        std::fs::write(&second, b"second").unwrap();
        let env = OsString::from(format!(
            " file://{} , ,file://{} ",
            first.display(),
            second.display()
        ));
        let accepted = AcceptedEgressSources::parse_os(&[], Some(&env), directory.path()).unwrap();
        let first = CanonicalSource(CanonicalSourceKind::File(PathBuf::from("first.wasm")));
        let second = CanonicalSource(CanonicalSourceKind::File(PathBuf::from("second.wasm")));

        assert!(accepted.accepts(&first).is_some());
        assert!(accepted.accepts(&second).is_some());

        let project_relative =
            AcceptedEgressSources::parse_os(&["file://first.wasm".into()], None, directory.path())
                .unwrap();
        assert!(project_relative.accepts(&first).is_some());
    }

    #[test]
    fn explicit_consent_evidence_comes_from_prompt_or_matching_accept_list() {
        let source = CanonicalSource(CanonicalSourceKind::File(PathBuf::from("first.wasm")));
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("first.wasm");
        std::fs::write(&file, b"provider").unwrap();
        let accepted = AcceptedEgressSources::parse_os(
            &[format!("file://{}", file.display())],
            None,
            directory.path(),
        )
        .unwrap();
        assert!(accepted.accepts(&source).is_some());

        let other = CanonicalSource(CanonicalSourceKind::File(PathBuf::from("other.wasm")));
        assert!(accepted.accepts(&other).is_none());

        let mut yes = Cursor::new(b"yes\n");
        let mut output = Vec::new();
        assert!(
            super::prompt_for_explicit_consent(&source, &mut yes, &mut output)
                .unwrap()
                .is_some()
        );
        let mut no = Cursor::new(b"no\n");
        assert!(
            super::prompt_for_explicit_consent(&source, &mut no, &mut Vec::new())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn invalid_locked_pattern_does_not_render_control_characters_raw() {
        let rendered = super::EgressConsentError::InvalidLockedPattern {
            source: "unsafe\u{1b}\u{7}/source".into(),
            pattern: "bad\u{1b}\u{7}.example.com".into(),
            detail: "invalid fixture pattern".into(),
            project_dir: PathBuf::from("."),
        }
        .to_string();

        assert!(!rendered.contains('\u{1b}'), "{rendered:?}");
        assert!(!rendered.contains('\u{7}'), "{rendered:?}");
        assert_eq!(rendered.matches(r"\u{1b}").count(), 2, "{rendered:?}");
        assert_eq!(rendered.matches(r"\u{7}").count(), 2, "{rendered:?}");
    }

    #[test]
    fn invalid_accepted_source_does_not_render_control_characters_or_value_twice() {
        let value = "not-a-source\u{1b}\u{7}".to_string();
        let error =
            AcceptedEgressSources::parse_os(std::slice::from_ref(&value), None, Path::new("."))
                .unwrap_err();
        let rendered = error.to_string();

        assert!(!rendered.contains('\u{1b}'), "{rendered:?}");
        assert!(!rendered.contains('\u{7}'), "{rendered:?}");
        assert_eq!(rendered.matches(r"\u{1b}").count(), 1, "{rendered:?}");
        assert_eq!(rendered.matches(r"\u{7}").count(), 1, "{rendered:?}");
    }
}
