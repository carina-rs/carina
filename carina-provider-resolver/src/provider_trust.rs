use std::fmt;

use carina_core::provider_source::RemoteSourceKey;

use crate::provider_resolver::RegistrySignatureProtection;

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

    pub(crate) fn from_verified_remote(
        source: &RemoteSourceKey,
        signature: Option<&RegistrySignatureProtection>,
    ) -> Self {
        if !signer_is_carina_rs(signature) {
            return Self::untrusted();
        }

        match source.as_str() {
            "github.com/carina-rs/carina-provider-aws" | "carina-rs/aws" => Self {
                class: ProviderTrustClass::Aws,
                name_requirement: ProviderNameRequirement::Exact("aws"),
            },
            "github.com/carina-rs/carina-provider-awscc" | "carina-rs/awscc" => Self {
                class: ProviderTrustClass::Aws,
                name_requirement: ProviderNameRequirement::Exact("awscc"),
            },
            "github.com/carina-rs/carina-provider-github" | "carina-rs/github" => Self {
                class: ProviderTrustClass::GitHub,
                name_requirement: ProviderNameRequirement::Exact("github"),
            },
            _ => Self::untrusted(),
        }
    }

    pub(crate) fn trusted_local(class: ProviderTrustClass) -> Self {
        Self {
            class,
            name_requirement: match class {
                ProviderTrustClass::Aws => ProviderNameRequirement::AwsFamily,
                ProviderTrustClass::GitHub => ProviderNameRequirement::Exact("github"),
                ProviderTrustClass::Untrusted => ProviderNameRequirement::Any,
            },
        }
    }

    pub(crate) fn untrusted() -> Self {
        Self {
            class: ProviderTrustClass::Untrusted,
            name_requirement: ProviderNameRequirement::Any,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct ProviderLoadIdentity {
    configured_name: String,
    trust: ProviderTrust,
}

impl ProviderLoadIdentity {
    pub fn configured_name(&self) -> &str {
        &self.configured_name
    }

    pub fn trust(&self) -> &ProviderTrust {
        &self.trust
    }

    pub(crate) fn new(
        configured_name: String,
        trust: ProviderTrust,
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
            return Err(ProviderLoadIdentityError {
                configured_name,
                expected,
            });
        }
        Ok(Self {
            configured_name,
            trust,
        })
    }

    #[cfg(feature = "test-utils")]
    pub fn for_test(configured_name: impl Into<String>, class: ProviderTrustClass) -> Self {
        Self {
            configured_name: configured_name.into(),
            trust: ProviderTrust {
                class,
                name_requirement: ProviderNameRequirement::Any,
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderLoadIdentityError {
    configured_name: String,
    expected: &'static str,
}

impl ProviderLoadIdentityError {
    pub fn configured_name(&self) -> &str {
        &self.configured_name
    }

    pub fn expected(&self) -> &str {
        self.expected
    }
}

impl fmt::Display for ProviderLoadIdentityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "configured provider name '{}' does not match trusted provider identity {}",
            self.configured_name, self.expected
        )
    }
}

impl std::error::Error for ProviderLoadIdentityError {}

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
    fn evil_github_aws_source_is_untrusted() {
        let trust = ProviderTrust::from_verified_remote(&source_key("github.com/evil/aws"), None);

        assert_eq!(trust.class(), ProviderTrustClass::Untrusted);
    }

    #[test]
    fn non_official_registry_aws_sources_are_untrusted() {
        for source in [
            "registry.example.com/carina-rs/aws",
            "registry.carina-rs.dev/evil/aws",
            "carina-rs/Aws",
            "github.com/Carina-rs/carina-provider-aws",
            "github.com/carina-rs/Carina-provider-aws",
        ] {
            let trust = ProviderTrust::from_verified_remote(&source_key(source), None);
            assert_eq!(
                trust.class(),
                ProviderTrustClass::Untrusted,
                "source {source} must fail closed"
            );
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
            let trust = ProviderTrust::from_verified_remote(&source_key(source), None);
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
            let trust = ProviderTrust::from_verified_remote(&source_key(source), None);
            assert_eq!(trust.class(), ProviderTrustClass::GitHub);
        }
    }

    #[test]
    fn official_source_with_non_carina_signer_pin_is_untrusted() {
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
            let trust = ProviderTrust::from_verified_remote(&source, Some(signature));
            assert_eq!(trust.class(), ProviderTrustClass::Untrusted);
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
            let trust = ProviderTrust::from_verified_remote(&source, Some(signature));
            assert_eq!(trust.class(), ProviderTrustClass::Aws);
        }
    }

    #[test]
    fn trusted_remote_identity_rejects_disagreeing_configured_name() {
        let trust = ProviderTrust::from_verified_remote(
            &source_key("github.com/carina-rs/carina-provider-aws"),
            None,
        );

        let error = ProviderLoadIdentity::new("foo".into(), trust).unwrap_err();

        assert_eq!(error.configured_name, "foo");
        assert_eq!(error.expected, "aws");
    }

    #[test]
    fn untrusted_identity_accepts_any_configured_name_without_gaining_privilege() {
        let identity = ProviderLoadIdentity::new(
            "aws".into(),
            ProviderTrust::from_verified_remote(&source_key("github.com/evil/aws"), None),
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
