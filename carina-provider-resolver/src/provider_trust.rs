use std::fmt;
use std::path::{Path, PathBuf};

use carina_core::hint::ProjectCommand;
use carina_core::provider_source::RemoteSourceKey;

use crate::provider_resolver::RegistrySignatureProtection;
use crate::user_trust::{UserTrustRecordError, UserVerifiedIdentity};

const CARINA_RS_GITHUB_ORG_ID: &str = "260157389";
const CARINA_RS_GITHUB_SAN_PREFIX: &str = "https://github.com/carina-rs/";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderTrustClass {
    Aws,
    GitHub,
    Untrusted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderTrust {
    class: ProviderTrustClass,
    name_requirement: ProviderNameRequirement,
    carina_rs_signed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ProviderNameRequirement {
    Any,
    Exact(&'static str),
    AwsFamily,
}

impl ProviderTrust {
    pub fn class(&self) -> ProviderTrustClass {
        self.class
    }

    pub(crate) fn is_carina_rs_signed(&self) -> bool {
        self.carina_rs_signed
    }

    pub(crate) fn from_verified_remote(identity: &UserVerifiedIdentity) -> Option<Self> {
        classify_verified_remote(identity.source(), identity.signer())
    }

    pub(crate) fn remote_source_is_allowlisted(source: &RemoteSourceKey) -> bool {
        remote_trust_shape(source).is_some()
    }

    pub(crate) fn trusted_local(class: ProviderTrustClass) -> Self {
        Self {
            class,
            name_requirement: match class {
                ProviderTrustClass::Aws => ProviderNameRequirement::AwsFamily,
                ProviderTrustClass::GitHub => ProviderNameRequirement::Exact("github"),
                ProviderTrustClass::Untrusted => ProviderNameRequirement::Any,
            },
            carina_rs_signed: false,
        }
    }

    pub(crate) fn untrusted() -> Self {
        Self {
            class: ProviderTrustClass::Untrusted,
            name_requirement: ProviderNameRequirement::Any,
            carina_rs_signed: false,
        }
    }
}

fn classify_verified_remote(
    source: &RemoteSourceKey,
    signature: Option<&RegistrySignatureProtection>,
) -> Option<ProviderTrust> {
    if !signer_is_carina_rs(signature) {
        return None;
    }
    remote_trust_shape(source).map(|(class, name_requirement)| ProviderTrust {
        class,
        name_requirement,
        carina_rs_signed: true,
    })
}

fn remote_trust_shape(
    source: &RemoteSourceKey,
) -> Option<(ProviderTrustClass, ProviderNameRequirement)> {
    match source.as_str() {
        "github.com/carina-rs/carina-provider-aws" | "carina-rs/aws" => Some((
            ProviderTrustClass::Aws,
            ProviderNameRequirement::Exact("aws"),
        )),
        "github.com/carina-rs/carina-provider-awscc" | "carina-rs/awscc" => Some((
            ProviderTrustClass::Aws,
            ProviderNameRequirement::Exact("awscc"),
        )),
        "github.com/carina-rs/carina-provider-github" | "carina-rs/github" => Some((
            ProviderTrustClass::GitHub,
            ProviderNameRequirement::Exact("github"),
        )),
        _ => None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderLoadSource {
    LocalFile {
        path: PathBuf,
        canonical_source: PathBuf,
    },
    Remote {
        source: RemoteSourceKey,
    },
}

impl fmt::Display for ProviderLoadSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProviderLoadSource::LocalFile {
                path,
                canonical_source: _,
            } => {
                write!(formatter, "file://{}", path.display())
            }
            ProviderLoadSource::Remote { source } => source.fmt(formatter),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct ProviderLoadIdentity {
    configured_name: String,
    trust: ProviderTrust,
    source: ProviderLoadSource,
}

impl ProviderLoadIdentity {
    pub fn configured_name(&self) -> &str {
        &self.configured_name
    }

    pub fn trust(&self) -> &ProviderTrust {
        &self.trust
    }

    pub fn source(&self) -> &ProviderLoadSource {
        &self.source
    }

    pub(crate) fn new(
        configured_name: String,
        trust: ProviderTrust,
        source: ProviderLoadSource,
    ) -> Result<Self, ProviderLoadIdentityError> {
        let expected = match trust.name_requirement {
            ProviderNameRequirement::Any => None,
            ProviderNameRequirement::Exact(expected) if configured_name != expected => {
                Some(expected)
            }
            ProviderNameRequirement::Exact(_) => None,
            ProviderNameRequirement::AwsFamily
                if !matches!(configured_name.as_str(), "aws" | "awscc") =>
            {
                Some("aws or awscc")
            }
            ProviderNameRequirement::AwsFamily => None,
        };
        if let Some(expected) = expected {
            return Err(ProviderLoadIdentityError::ConfiguredNameMismatch {
                configured_name,
                expected,
            });
        }
        Ok(Self {
            configured_name,
            trust,
            source,
        })
    }

    #[cfg(any(test, feature = "test-utils"))]
    pub fn for_test(configured_name: impl Into<String>, class: ProviderTrustClass) -> Self {
        let configured_name = configured_name.into();
        let source = ProviderLoadSource::LocalFile {
            path: PathBuf::from(format!("/test-providers/{configured_name}.wasm")),
            canonical_source: PathBuf::from(format!("{configured_name}.wasm")),
        };
        Self {
            configured_name,
            trust: ProviderTrust {
                class,
                name_requirement: ProviderNameRequirement::Any,
                carina_rs_signed: false,
            },
            source,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnverifiedOfficialProviderReason {
    MissingRecord,
    InvalidRecord,
    SignerNotCarinaRs,
}

#[derive(Debug)]
enum UnverifiedOfficialProviderFailure {
    MissingRecord,
    InvalidRecord(UserTrustRecordError),
    SignerNotCarinaRs,
}

/// An allow-listed provider source whose user-owned verification is absent or
/// unusable. This state never produces a [`ProviderLoadIdentity`].
#[derive(Debug)]
pub struct UnverifiedOfficialProviderError {
    source: RemoteSourceKey,
    project_dir: PathBuf,
    failure: UnverifiedOfficialProviderFailure,
}

impl UnverifiedOfficialProviderError {
    pub fn source_key(&self) -> &RemoteSourceKey {
        &self.source
    }

    pub fn reason(&self) -> UnverifiedOfficialProviderReason {
        match &self.failure {
            UnverifiedOfficialProviderFailure::MissingRecord => {
                UnverifiedOfficialProviderReason::MissingRecord
            }
            UnverifiedOfficialProviderFailure::InvalidRecord(_) => {
                UnverifiedOfficialProviderReason::InvalidRecord
            }
            UnverifiedOfficialProviderFailure::SignerNotCarinaRs => {
                UnverifiedOfficialProviderReason::SignerNotCarinaRs
            }
        }
    }

    fn missing_record(source: &RemoteSourceKey, project_dir: &Path) -> Self {
        Self {
            source: source.clone(),
            project_dir: project_dir.to_path_buf(),
            failure: UnverifiedOfficialProviderFailure::MissingRecord,
        }
    }

    fn invalid_record(
        source: &RemoteSourceKey,
        project_dir: &Path,
        error: UserTrustRecordError,
    ) -> Self {
        Self {
            source: source.clone(),
            project_dir: project_dir.to_path_buf(),
            failure: UnverifiedOfficialProviderFailure::InvalidRecord(error),
        }
    }

    fn signer_not_carina_rs(source: &RemoteSourceKey, project_dir: &Path) -> Self {
        Self {
            source: source.clone(),
            project_dir: project_dir.to_path_buf(),
            failure: UnverifiedOfficialProviderFailure::SignerNotCarinaRs,
        }
    }
}

impl fmt::Display for UnverifiedOfficialProviderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let command = ProjectCommand::new("init", &self.project_dir);
        match &self.failure {
            UnverifiedOfficialProviderFailure::MissingRecord => write!(
                formatter,
                "{} is a carina-rs provider but this user hasn't verified it; run `{command}`",
                self.source
            ),
            UnverifiedOfficialProviderFailure::InvalidRecord(error) => {
                let record_problem = error.display_chain();
                write!(
                    formatter,
                    "{} is a carina-rs provider but this user's verification record is unusable: {record_problem}; run `{command}`",
                    self.source
                )
            }
            UnverifiedOfficialProviderFailure::SignerNotCarinaRs => write!(
                formatter,
                "{} is a carina-rs provider but this user's verification record signer is not carina-rs; run `{command}`",
                self.source
            ),
        }
    }
}

// `Display` embeds the complete trust-record failure chain so CLI and LSP
// callers receive one self-contained diagnostic. Exposing the same child via
// `source()` would make standard chain walkers render it again.
impl std::error::Error for UnverifiedOfficialProviderError {}

#[derive(Debug)]
pub enum ProviderLoadIdentityError {
    ConfiguredNameMismatch {
        configured_name: String,
        expected: &'static str,
    },
    UnverifiedOfficial(UnverifiedOfficialProviderError),
}

impl ProviderLoadIdentityError {
    pub(crate) fn missing_official_record(source: &RemoteSourceKey, project_dir: &Path) -> Self {
        Self::UnverifiedOfficial(UnverifiedOfficialProviderError::missing_record(
            source,
            project_dir,
        ))
    }

    pub(crate) fn invalid_official_record(
        source: &RemoteSourceKey,
        project_dir: &Path,
        error: UserTrustRecordError,
    ) -> Self {
        Self::UnverifiedOfficial(UnverifiedOfficialProviderError::invalid_record(
            source,
            project_dir,
            error,
        ))
    }

    pub(crate) fn non_carina_official_signer(source: &RemoteSourceKey, project_dir: &Path) -> Self {
        Self::UnverifiedOfficial(UnverifiedOfficialProviderError::signer_not_carina_rs(
            source,
            project_dir,
        ))
    }

    pub fn name_mismatch(&self) -> Option<(&str, &str)> {
        match self {
            Self::ConfiguredNameMismatch {
                configured_name,
                expected,
            } => Some((configured_name, expected)),
            Self::UnverifiedOfficial(_) => None,
        }
    }

    pub fn unverified_official(&self) -> Option<&UnverifiedOfficialProviderError> {
        match self {
            Self::ConfiguredNameMismatch { .. } => None,
            Self::UnverifiedOfficial(error) => Some(error),
        }
    }

    pub fn into_unverified_official(self) -> Result<UnverifiedOfficialProviderError, Self> {
        match self {
            Self::UnverifiedOfficial(error) => Ok(error),
            mismatch @ Self::ConfiguredNameMismatch { .. } => Err(mismatch),
        }
    }
}

impl fmt::Display for ProviderLoadIdentityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ConfiguredNameMismatch {
                configured_name,
                expected,
            } => write!(
                formatter,
                "configured provider name '{configured_name}' does not match trusted provider identity {expected}"
            ),
            Self::UnverifiedOfficial(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for ProviderLoadIdentityError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::ConfiguredNameMismatch { .. } => None,
            Self::UnverifiedOfficial(error) => Some(error),
        }
    }
}

fn signer_is_carina_rs(signature: Option<&RegistrySignatureProtection>) -> bool {
    use crate::provider_resolver::SignerPinState;

    match signature {
        None
        | Some(RegistrySignatureProtection::NotRequired)
        | Some(RegistrySignatureProtection::Required(SignerPinState::Unpinned)) => true,
        Some(RegistrySignatureProtection::Required(SignerPinState::Legacy(pin))) => pin
            .certificate_san
            .strip_prefix(CARINA_RS_GITHUB_SAN_PREFIX)
            .is_some_and(|suffix| !suffix.is_empty()),
        Some(RegistrySignatureProtection::Required(SignerPinState::Current(pins))) => {
            pins.source.repository_owner_id == CARINA_RS_GITHUB_ORG_ID
        }
    }
}

#[cfg(test)]
mod tests {
    use carina_core::provider_source::{ProviderSource, RemoteSource};

    use super::*;
    use crate::provider_resolver::{
        CurrentSignerPins, LegacySignerPin, SignerPinState, SourcePin, WorkflowPin, WorkflowPins,
    };

    fn source_key(source: &str) -> RemoteSourceKey {
        match source.parse::<ProviderSource>().unwrap() {
            ProviderSource::Remote(source) => source.source_key(),
            ProviderSource::File(_) => panic!("expected remote source"),
        }
    }

    fn current_signer(owner_id: &str) -> RegistrySignatureProtection {
        RegistrySignatureProtection::Required(SignerPinState::Current(CurrentSignerPins {
            source: SourcePin {
                oidc_issuer: "https://token.actions.githubusercontent.com".into(),
                repository_owner_id: owner_id.into(),
                repository_id: "123".into(),
            },
            workflows: WorkflowPins::Stable(WorkflowPin("release.yml".into())),
        }))
    }

    #[test]
    fn evil_github_aws_source_has_no_official_classification() {
        let trust = classify_verified_remote(&source_key("github.com/evil/aws"), None);

        assert!(trust.is_none());
    }

    #[test]
    fn non_official_registry_aws_sources_have_no_official_classification() {
        for source in [
            "registry.example.com/carina-rs/aws",
            "registry.carina-rs.dev/evil/aws",
            "carina-rs/Aws",
            "github.com/Carina-rs/carina-provider-aws",
            "github.com/carina-rs/Carina-provider-aws",
        ] {
            let trust = classify_verified_remote(&source_key(source), None);
            assert!(trust.is_none(), "source {source} must fail closed");
        }
    }

    #[test]
    fn official_github_and_registry_aws_sources_are_trusted() {
        for source in [
            "github.com/carina-rs/carina-provider-aws",
            "github.com/carina-rs/carina-provider-awscc",
            "carina-rs/aws",
            "registry.carina-rs.dev/carina-rs/awscc",
        ] {
            let trust = classify_verified_remote(&source_key(source), None).unwrap();
            assert_eq!(
                trust.class(),
                ProviderTrustClass::Aws,
                "source {source} should be trusted as AWS"
            );
        }
    }

    #[test]
    fn official_github_provider_source_is_trusted_as_github() {
        for source in [
            "github.com/carina-rs/carina-provider-github",
            "carina-rs/github",
        ] {
            let trust = classify_verified_remote(&source_key(source), None).unwrap();
            assert_eq!(trust.class(), ProviderTrustClass::GitHub);
        }
    }

    #[test]
    fn official_source_with_non_carina_signer_pin_has_no_loadable_trust() {
        let source = source_key("carina-rs/aws");
        let signatures = [
            current_signer("999"),
            RegistrySignatureProtection::Required(SignerPinState::Legacy(LegacySignerPin {
                certificate_san:
                    "https://github.com/evil/aws/.github/workflows/release.yml@refs/heads/main"
                        .into(),
                oidc_issuer: "https://token.actions.githubusercontent.com".into(),
            })),
        ];

        for signature in &signatures {
            let trust = classify_verified_remote(&source, Some(signature));
            assert!(trust.is_none());
        }
    }

    #[test]
    fn official_source_with_carina_signer_pins_remains_trusted() {
        let source = source_key("carina-rs/aws");
        let signatures = [
            current_signer("260157389"),
            RegistrySignatureProtection::Required(SignerPinState::Legacy(LegacySignerPin {
                certificate_san: "https://github.com/carina-rs/carina-provider-aws/.github/workflows/release.yml@refs/heads/main".into(),
                oidc_issuer: "https://token.actions.githubusercontent.com".into(),
            })),
            RegistrySignatureProtection::Required(SignerPinState::Unpinned),
            RegistrySignatureProtection::NotRequired,
        ];

        for signature in &signatures {
            let trust = classify_verified_remote(&source, Some(signature)).unwrap();
            assert_eq!(trust.class(), ProviderTrustClass::Aws);
        }
    }

    #[test]
    fn trusted_remote_identity_rejects_disagreeing_configured_name() {
        let source = source_key("github.com/carina-rs/carina-provider-aws");
        let trust = classify_verified_remote(&source, None).unwrap();

        let error =
            ProviderLoadIdentity::new("foo".into(), trust, ProviderLoadSource::Remote { source })
                .unwrap_err();

        assert_eq!(error.name_mismatch(), Some(("foo", "aws")));
    }

    #[test]
    fn untrusted_identity_accepts_any_configured_name_without_gaining_privilege() {
        let identity = ProviderLoadIdentity::new(
            "aws".into(),
            ProviderTrust::untrusted(),
            ProviderLoadSource::LocalFile {
                path: PathBuf::from("/tmp/aws.wasm"),
                canonical_source: PathBuf::from("aws.wasm"),
            },
        )
        .unwrap();

        assert_eq!(identity.configured_name(), "aws");
        assert_eq!(identity.trust().class(), ProviderTrustClass::Untrusted);
    }

    #[test]
    fn normalized_default_registry_host_uses_the_short_source_key() {
        let explicit = source_key("registry.carina-rs.dev/carina-rs/aws");
        let short = source_key("carina-rs/aws");

        assert_eq!(explicit, short);
        assert_eq!(explicit.as_str(), "carina-rs/aws");
        assert!(matches!(
            "carina-rs/aws".parse::<ProviderSource>().unwrap(),
            ProviderSource::Remote(RemoteSource::Registry(_))
        ));
    }
}
