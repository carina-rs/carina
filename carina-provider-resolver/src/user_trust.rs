use std::error::Error;
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use carina_core::provider_source::RemoteSourceKey;
use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::provider_resolver::RegistrySignatureProtection;
use crate::verified_artifact::OriginDownloadProof;

const TRUST_RECORD_DIR_ENV: &str = "CARINA_TRUST_RECORD_DIR";

/// A user-owned attestation that this exact provider content was downloaded
/// from its canonical origin.
///
/// Its fields and write operation are private, and its crate-private
/// constructor requires the origin-download proof produced by the resolver:
///
/// ```compile_fail
/// use carina_provider_resolver::UserTrustRecord;
///
/// let record = UserTrustRecord {
///     source: "github.com/carina-rs/carina-provider-aws".into(),
///     sha256: "00".repeat(32),
///     signer: None,
///     recorded_at: "2026-01-01T00:00:00Z".into(),
/// };
/// record.write().unwrap();
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserTrustRecord {
    source: String,
    sha256: String,
    signer: Option<RegistrySignatureProtection>,
    recorded_at: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct UserTrustRecordSerde {
    source: String,
    sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    signer: Option<RegistrySignatureProtection>,
    recorded_at: String,
}

impl From<&UserTrustRecord> for UserTrustRecordSerde {
    fn from(record: &UserTrustRecord) -> Self {
        Self {
            source: record.source.clone(),
            sha256: record.sha256.clone(),
            signer: record.signer.clone(),
            recorded_at: record.recorded_at.clone(),
        }
    }
}

impl From<UserTrustRecordSerde> for UserTrustRecord {
    fn from(record: UserTrustRecordSerde) -> Self {
        Self {
            source: record.source,
            sha256: record.sha256,
            signer: record.signer,
            recorded_at: record.recorded_at,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UserVerifiedIdentity {
    source: RemoteSourceKey,
    signer: Option<RegistrySignatureProtection>,
}

impl UserVerifiedIdentity {
    pub(crate) fn source(&self) -> &RemoteSourceKey {
        &self.source
    }

    pub(crate) fn signer(&self) -> Option<&RegistrySignatureProtection> {
        self.signer.as_ref()
    }
}

#[derive(Debug)]
pub(crate) struct UserTrustRecordError {
    message: String,
    source: Option<Box<dyn Error + Send + Sync>>,
}

impl UserTrustRecordError {
    fn message(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            source: None,
        }
    }

    fn with_source(message: impl Into<String>, source: impl Error + Send + Sync + 'static) -> Self {
        Self {
            message: message.into(),
            source: Some(Box::new(source)),
        }
    }
}

impl fmt::Display for UserTrustRecordError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for UserTrustRecordError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.source
            .as_deref()
            .map(|source| source as &(dyn Error + 'static))
    }
}

/// Get the user-owned provider trust-record directory.
///
/// Checks `CARINA_TRUST_RECORD_DIR` first, then falls back to
/// `~/.carina/trust/`. Returns `None` if the home directory cannot be
/// determined.
pub fn user_trust_record_dir() -> Option<PathBuf> {
    std::env::var_os(TRUST_RECORD_DIR_ENV)
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".carina").join("trust")))
}

impl UserTrustRecord {
    pub(crate) fn from_origin_download(
        proof: OriginDownloadProof,
    ) -> Result<Self, UserTrustRecordError> {
        let (source, sha256, signer) = proof.into_parts();
        let recorded_at = OffsetDateTime::now_utc()
            .format(&Rfc3339)
            .map_err(|error| {
                UserTrustRecordError::with_source(
                    "Failed to format provider trust-record timestamp",
                    error,
                )
            })?;
        Ok(Self {
            source: source.to_string(),
            sha256,
            signer,
            recorded_at,
        })
    }

    pub(crate) fn write(&self) -> Result<(), UserTrustRecordError> {
        let path = trust_record_path(&self.sha256, &self.source)?;
        let parent = path
            .parent()
            .expect("a trust-record path always has a digest directory");
        fs::create_dir_all(parent).map_err(|error| {
            UserTrustRecordError::with_source(
                format!(
                    "Failed to create provider trust-record directory {}",
                    parent.display()
                ),
                error,
            )
        })?;

        let mut contents =
            serde_json::to_vec_pretty(&UserTrustRecordSerde::from(self)).map_err(|error| {
                UserTrustRecordError::with_source(
                    "Failed to serialize provider trust record",
                    error,
                )
            })?;
        contents.push(b'\n');
        let staged = Self::stage(&contents, parent, &path)?;

        match staged.persist_noclobber(&path) {
            Ok(file) => {
                drop(file);
                sync_directory(parent);
                Ok(())
            }
            Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => {
                drop(error.file);
                let existing = Self::load_at(&path)?;
                if self.same_attestation(&existing) {
                    existing.validate_recorded_at(&path)?;
                    Ok(())
                } else {
                    let replacement = Self::stage(&contents, parent, &path)?;
                    let file = replacement.persist(&path).map_err(|error| {
                        UserTrustRecordError::with_source(
                            format!("Failed to replace provider trust record {}", path.display()),
                            error.error,
                        )
                    })?;
                    drop(file);
                    sync_directory(parent);
                    Ok(())
                }
            }
            Err(error) => Err(UserTrustRecordError::with_source(
                format!("Failed to publish provider trust record {}", path.display()),
                error.error,
            )),
        }
    }

    fn stage(
        contents: &[u8],
        parent: &Path,
        path: &Path,
    ) -> Result<NamedTempFile, UserTrustRecordError> {
        let mut staged = NamedTempFile::new_in(parent).map_err(|error| {
            UserTrustRecordError::with_source(
                format!(
                    "Failed to stage provider trust record in {}",
                    parent.display()
                ),
                error,
            )
        })?;
        staged.write_all(contents).map_err(|error| {
            UserTrustRecordError::with_source(
                format!(
                    "Failed to write staged provider trust record {}",
                    path.display()
                ),
                error,
            )
        })?;
        staged.as_file().sync_all().map_err(|error| {
            UserTrustRecordError::with_source(
                format!(
                    "Failed to sync staged provider trust record {}",
                    path.display()
                ),
                error,
            )
        })?;
        Ok(staged)
    }

    fn load_at(path: &Path) -> Result<Self, UserTrustRecordError> {
        let contents = fs::read(path).map_err(|error| {
            UserTrustRecordError::with_source(
                format!("Failed to read provider trust record {}", path.display()),
                error,
            )
        })?;
        serde_json::from_slice::<UserTrustRecordSerde>(&contents)
            .map(UserTrustRecord::from)
            .map_err(|error| {
                UserTrustRecordError::with_source(
                    format!("Failed to parse provider trust record {}", path.display()),
                    error,
                )
            })
    }

    fn same_attestation(&self, other: &Self) -> bool {
        self.source == other.source && self.sha256 == other.sha256 && self.signer == other.signer
    }

    fn validate(
        &self,
        expected_sha256: &str,
        expected_source: &RemoteSourceKey,
        path: &Path,
    ) -> Result<(), UserTrustRecordError> {
        if self.sha256 != expected_sha256 || self.source != expected_source.as_str() {
            return Err(UserTrustRecordError::message(format!(
                "Provider trust record {} does not match its requested source and sha256",
                path.display()
            )));
        }
        self.validate_recorded_at(path)
    }

    fn validate_recorded_at(&self, path: &Path) -> Result<(), UserTrustRecordError> {
        OffsetDateTime::parse(&self.recorded_at, &Rfc3339).map_err(|error| {
            UserTrustRecordError::with_source(
                format!(
                    "Provider trust record {} has an invalid recorded_at timestamp",
                    path.display()
                ),
                error,
            )
        })?;
        Ok(())
    }
}

pub(crate) fn load_user_verified_identity(
    sha256: &str,
    source: &RemoteSourceKey,
) -> Result<Option<UserVerifiedIdentity>, UserTrustRecordError> {
    let path = trust_record_path(sha256, source.as_str())?;
    let record = match UserTrustRecord::load_at(&path) {
        Ok(record) => record,
        Err(error)
            if error
                .source()
                .and_then(|source| source.downcast_ref::<io::Error>())
                .is_some_and(|source| source.kind() == io::ErrorKind::NotFound) =>
        {
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    record.validate(sha256, source, &path)?;
    Ok(Some(UserVerifiedIdentity {
        source: source.clone(),
        signer: record.signer,
    }))
}

pub(crate) fn user_trust_record_exists(sha256: &str, source: &RemoteSourceKey) -> bool {
    matches!(load_user_verified_identity(sha256, source), Ok(Some(_)))
}

fn trust_record_path(sha256: &str, source: &str) -> Result<PathBuf, UserTrustRecordError> {
    if sha256.len() != 64
        || !sha256
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(UserTrustRecordError::message(format!(
            "Invalid provider trust-record sha256 {sha256:?}"
        )));
    }
    let root = user_trust_record_dir().ok_or_else(|| {
        UserTrustRecordError::message(
            "Cannot determine the provider trust-record directory; set CARINA_TRUST_RECORD_DIR",
        )
    })?;
    let encoded_source = URL_SAFE_NO_PAD.encode(source.as_bytes());
    Ok(root.join(sha256).join(format!("{encoded_source}.json")))
}

fn sync_directory(path: &Path) {
    if let Ok(directory) = File::open(path) {
        let _ = directory.sync_all();
    }
}

#[cfg(test)]
mod tests {
    use carina_core::provider_source::ProviderSource;

    use super::*;
    use crate::provider_resolver::{LegacySignerPin, SignerPinState};
    use crate::verified_artifact::record_verified_origin_download;

    #[test]
    fn user_trust_record_write_replaces_changed_attestation() {
        let _env_guard = crate::test_env_guard();
        let trust = tempfile::tempdir().unwrap();
        let _trust_env = crate::ScopedEnvVar::set("CARINA_TRUST_RECORD_DIR", trust.path());
        let source = match "github.com/carina-rs/carina-provider-aws"
            .parse::<ProviderSource>()
            .unwrap()
        {
            ProviderSource::Remote(source) => source.source_key(),
            ProviderSource::File(_) => unreachable!(),
        };
        let sha256 = "01".repeat(32);
        let first_signer =
            RegistrySignatureProtection::Required(SignerPinState::Legacy(LegacySignerPin {
                certificate_san: "https://github.com/carina-rs/old-workflow".into(),
                oidc_issuer: "https://token.actions.githubusercontent.com".into(),
            }));
        let new_signer =
            RegistrySignatureProtection::Required(SignerPinState::Legacy(LegacySignerPin {
                certificate_san: "https://github.com/carina-rs/new-workflow".into(),
                oidc_issuer: "https://token.actions.githubusercontent.com".into(),
            }));

        record_verified_origin_download(&source, &sha256, Some(&first_signer)).unwrap();
        record_verified_origin_download(&source, &sha256, Some(&new_signer)).unwrap();

        let stored = load_user_verified_identity(&sha256, &source)
            .unwrap()
            .unwrap();
        assert_eq!(stored.signer(), Some(&new_signer));
    }
}
