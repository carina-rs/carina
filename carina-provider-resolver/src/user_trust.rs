use std::error::Error;
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use carina_core::UserDirs;
use carina_core::provider_source::RemoteSourceKey;
use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::provider_resolver::RegistrySignatureProtection;
use crate::verified_artifact::OriginDownloadProof;

const USER_TRUST_RECORD_VERSION: u32 = 1;

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
    version: u32,
    source: String,
    sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    signer: Option<RegistrySignatureProtection>,
    recorded_at: String,
}

impl From<&UserTrustRecord> for UserTrustRecordSerde {
    fn from(record: &UserTrustRecord) -> Self {
        Self {
            version: USER_TRUST_RECORD_VERSION,
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

    pub(crate) fn display_chain(&self) -> String {
        let mut rendered = self.to_string();
        let mut source = Error::source(self);
        while let Some(error) = source {
            rendered.push_str(": ");
            rendered.push_str(&error.to_string());
            source = error.source();
        }
        rendered
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

    pub(crate) fn write(&self, user_dirs: &UserDirs) -> Result<(), UserTrustRecordError> {
        let path = trust_record_path(user_dirs, &self.sha256, &self.source)?;
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
                let existing_contents = Self::read_at(&path)?;
                let existing = Self::parse_at(&path, &existing_contents).and_then(|record| {
                    record.validate_recorded_at(&path)?;
                    Ok(record)
                });
                match existing {
                    Ok(existing) if self.same_attestation(&existing) => Ok(()),
                    Ok(_) | Err(_) => Self::replace(&contents, parent, &path),
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

    fn replace(contents: &[u8], parent: &Path, path: &Path) -> Result<(), UserTrustRecordError> {
        let replacement = Self::stage(contents, parent, path)?;
        let file = replacement.persist(path).map_err(|error| {
            UserTrustRecordError::with_source(
                format!("Failed to replace provider trust record {}", path.display()),
                error.error,
            )
        })?;
        drop(file);
        sync_directory(parent);
        Ok(())
    }

    fn load_at(path: &Path) -> Result<Self, UserTrustRecordError> {
        let contents = Self::read_at(path)?;
        Self::parse_at(path, &contents)
    }

    fn read_at(path: &Path) -> Result<Vec<u8>, UserTrustRecordError> {
        fs::read(path).map_err(|error| {
            UserTrustRecordError::with_source(
                format!("Failed to read provider trust record {}", path.display()),
                error,
            )
        })
    }

    fn parse_at(path: &Path, contents: &[u8]) -> Result<Self, UserTrustRecordError> {
        let record = serde_json::from_slice::<UserTrustRecordSerde>(contents).map_err(|error| {
            UserTrustRecordError::with_source(
                format!("Failed to parse provider trust record {}", path.display()),
                error,
            )
        })?;
        if record.version != USER_TRUST_RECORD_VERSION {
            return Err(UserTrustRecordError::message(format!(
                "Failed to parse provider trust record {}: unsupported version {}",
                path.display(),
                record.version
            )));
        }
        Ok(UserTrustRecord::from(record))
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
    user_dirs: &UserDirs,
    sha256: &str,
    source: &RemoteSourceKey,
) -> Result<Option<UserVerifiedIdentity>, UserTrustRecordError> {
    let path = trust_record_path(user_dirs, sha256, source.as_str())?;
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

pub(crate) fn user_trust_record_exists(
    user_dirs: &UserDirs,
    sha256: &str,
    source: &RemoteSourceKey,
) -> bool {
    matches!(
        load_user_verified_identity(user_dirs, sha256, source),
        Ok(Some(_))
    )
}

fn trust_record_path(
    user_dirs: &UserDirs,
    sha256: &str,
    source: &str,
) -> Result<PathBuf, UserTrustRecordError> {
    if sha256.len() != 64
        || !sha256
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(UserTrustRecordError::message(format!(
            "Invalid provider trust-record sha256 {sha256:?}"
        )));
    }
    let encoded_source = URL_SAFE_NO_PAD.encode(source.as_bytes());
    Ok(user_dirs
        .trust_record_dir()
        .join(sha256)
        .join(format!("{encoded_source}.json")))
}

fn sync_directory(path: &Path) {
    if let Ok(directory) = File::open(path) {
        let _ = directory.sync_all();
    }
}

#[cfg(test)]
mod tests {
    use carina_core::UserDirs;
    use carina_core::provider_source::ProviderSource;

    use super::*;
    use crate::provider_resolver::{LegacySignerPin, SignerPinState};
    use crate::verified_artifact::record_test_github_origin_download;

    fn github_source_key() -> RemoteSourceKey {
        match "github.com/carina-rs/carina-provider-aws"
            .parse::<ProviderSource>()
            .unwrap()
        {
            ProviderSource::Remote(source) => source.source_key(),
            ProviderSource::File(_) => unreachable!(),
        }
    }

    #[test]
    fn user_trust_record_write_uses_the_supplied_user_dirs() {
        let root = tempfile::tempdir().unwrap();
        let user_dirs = UserDirs::under(root.path());
        let source = github_source_key();
        let sha256 = "04".repeat(32);

        record_test_github_origin_download(&user_dirs, &source, &sha256, None).unwrap();

        let record = trust_record_path(&user_dirs, &sha256, source.as_str()).unwrap();
        assert!(record.is_file());
        assert!(record.starts_with(user_dirs.trust_record_dir()));
        assert!(record.starts_with(root.path()));
    }

    #[test]
    fn user_trust_record_write_replaces_changed_attestation() {
        let user_home = tempfile::tempdir().unwrap();
        let user_dirs = UserDirs::under(user_home.path());
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

        record_test_github_origin_download(&user_dirs, &source, &sha256, Some(&first_signer))
            .unwrap();
        record_test_github_origin_download(&user_dirs, &source, &sha256, Some(&new_signer))
            .unwrap();

        let stored = load_user_verified_identity(&user_dirs, &sha256, &source)
            .unwrap()
            .unwrap();
        assert_eq!(stored.signer(), Some(&new_signer));
    }

    #[test]
    fn user_trust_record_serializes_version_one() {
        let user_home = tempfile::tempdir().unwrap();
        let user_dirs = UserDirs::under(user_home.path());
        let source = github_source_key();
        let sha256 = "02".repeat(32);

        record_test_github_origin_download(&user_dirs, &source, &sha256, None).unwrap();

        let path = trust_record_path(&user_dirs, &sha256, source.as_str()).unwrap();
        let stored: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(stored["version"], 1);
    }

    #[test]
    fn user_trust_record_write_replaces_unparsable_existing_record() {
        let user_home = tempfile::tempdir().unwrap();
        let user_dirs = UserDirs::under(user_home.path());
        let source = github_source_key();
        let sha256 = "03".repeat(32);
        record_test_github_origin_download(&user_dirs, &source, &sha256, None).unwrap();
        let path = trust_record_path(&user_dirs, &sha256, source.as_str()).unwrap();

        for broken in [
            b"{not json".to_vec(),
            format!(
                r#"{{"version":2,"source":{:?},"sha256":{:?},"recorded_at":"2026-01-01T00:00:00Z"}}"#,
                source.as_str(), sha256
            )
            .into_bytes(),
            format!(
                r#"{{"version":1,"source":{:?},"sha256":{:?},"recorded_at":"2026-01-01T00:00:00Z","unexpected":true}}"#,
                source.as_str(), sha256
            )
            .into_bytes(),
            format!(
                r#"{{"version":1,"source":{:?},"sha256":{:?},"recorded_at":"not-a-timestamp"}}"#,
                source.as_str(), sha256
            )
            .into_bytes(),
        ] {
            std::fs::write(&path, &broken).unwrap();

            record_test_github_origin_download(&user_dirs, &source, &sha256, None)
                .expect("a fresh origin proof must replace an unparsable record");

            assert!(load_user_verified_identity(&user_dirs, &sha256, &source)
                .unwrap()
                .is_some());
        }
    }
}
