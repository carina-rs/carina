use std::fmt;
use std::path::{Path, PathBuf};

use crate::provider_trust::{ProviderTrust, ProviderTrustClass};

#[derive(Debug, Clone, PartialEq, Eq)]
enum GrantOrigin {
    CommandLine,
    Environment,
}

impl GrantOrigin {
    fn label(&self) -> &'static str {
        match self {
            Self::CommandLine => "--trust-local-provider",
            Self::Environment => "CARINA_TRUST_LOCAL_PROVIDER",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TrustedLocalGrant {
    canonical_path: PathBuf,
    class: ProviderTrustClass,
    origin: GrantOrigin,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustedLocalGrants(Vec<TrustedLocalGrant>);

impl TrustedLocalGrants {
    pub fn parse(cli_values: &[String], env_value: Option<&str>) -> Result<Self, GrantParseError> {
        let mut grants = Vec::new();
        for value in cli_values {
            grants.push(parse_grant(value, GrantOrigin::CommandLine)?);
        }
        if let Some(env_value) = env_value.filter(|value| !value.is_empty()) {
            for value in std::env::split_paths(env_value) {
                let value = value
                    .into_os_string()
                    .into_string()
                    .map_err(|value| GrantParseError {
                    message: format!(
                        "invalid CARINA_TRUST_LOCAL_PROVIDER entry {:?}: PATH=KIND must be valid UTF-8",
                        value
                    ),
                })?;
                grants.push(parse_grant(&value, GrantOrigin::Environment)?);
            }
        }
        Ok(Self(grants))
    }

    pub(crate) fn matching(&self, canonical_path: &Path) -> Option<MatchedLocalGrant<'_>> {
        self.0
            .iter()
            .find(|grant| grant.canonical_path == canonical_path)
            .map(|grant| MatchedLocalGrant { grant })
    }
}

pub(crate) struct MatchedLocalGrant<'a> {
    grant: &'a TrustedLocalGrant,
}

impl MatchedLocalGrant<'_> {
    pub(crate) fn trust(&self) -> ProviderTrust {
        ProviderTrust::trusted_local(self.grant.class)
    }

    pub(crate) fn notice(&self) -> String {
        format!(
            "carina: trusting local provider {} as {} ({})",
            self.grant.canonical_path.display(),
            trust_class_name(self.grant.class),
            self.grant.origin.label(),
        )
    }
}

#[derive(Debug)]
pub struct GrantParseError {
    message: String,
}

impl fmt::Display for GrantParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for GrantParseError {}

fn parse_grant(value: &str, origin: GrantOrigin) -> Result<TrustedLocalGrant, GrantParseError> {
    let (path, kind) = value.rsplit_once('=').ok_or_else(|| GrantParseError {
        message: format!("invalid trusted local provider grant {value:?}: expected PATH=KIND"),
    })?;
    let path = PathBuf::from(path);
    if !path.is_absolute() {
        return Err(GrantParseError {
            message: format!(
                "invalid trusted local provider grant {value:?}: provider path must be absolute"
            ),
        });
    }
    if path.extension().is_none_or(|extension| extension != "wasm") {
        return Err(GrantParseError {
            message: format!(
                "invalid trusted local provider grant {value:?}: provider path must name a .wasm file"
            ),
        });
    }
    let canonical_path = std::fs::canonicalize(&path).map_err(|error| GrantParseError {
        message: format!(
            "invalid trusted local provider grant {value:?}: failed to canonicalize {}: {error}",
            path.display()
        ),
    })?;
    if !canonical_path.is_file() {
        return Err(GrantParseError {
            message: format!(
                "invalid trusted local provider grant {value:?}: {} is not a file",
                canonical_path.display()
            ),
        });
    }
    let class = match kind {
        "aws" => ProviderTrustClass::Aws,
        "github" => ProviderTrustClass::GitHub,
        _ => {
            return Err(GrantParseError {
                message: format!(
                    "invalid trusted local provider grant {value:?}: KIND must be aws or github"
                ),
            });
        }
    };
    Ok(TrustedLocalGrant {
        canonical_path,
        class,
        origin,
    })
}

fn trust_class_name(class: ProviderTrustClass) -> &'static str {
    match class {
        ProviderTrustClass::Aws => "aws",
        ProviderTrustClass::GitHub => "github",
        ProviderTrustClass::Untrusted => "untrusted",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grant(path: &Path, kind: &str) -> String {
        format!("{}={kind}", path.display())
    }

    #[test]
    fn parses_two_environment_grants_with_platform_path_separator() {
        let temp = tempfile::tempdir().unwrap();
        let aws = temp.path().join("aws.wasm");
        let github = temp.path().join("github.wasm");
        std::fs::write(&aws, b"aws").unwrap();
        std::fs::write(&github, b"github").unwrap();
        let env = std::env::join_paths([grant(&aws, "aws"), grant(&github, "github")]).unwrap();

        let grants = TrustedLocalGrants::parse(&[], env.to_str()).unwrap();

        let aws_path = std::fs::canonicalize(&aws).unwrap();
        let github_path = std::fs::canonicalize(&github).unwrap();
        assert_eq!(
            grants.matching(&aws_path).unwrap().trust().class(),
            ProviderTrustClass::Aws
        );
        assert_eq!(
            grants.matching(&github_path).unwrap().trust().class(),
            ProviderTrustClass::GitHub
        );
    }

    #[test]
    fn grant_parser_rejects_relative_path() {
        let error =
            TrustedLocalGrants::parse(&["relative/provider.wasm=aws".into()], None).unwrap_err();

        assert!(error.to_string().contains("absolute"));
    }

    #[test]
    fn grant_parser_rejects_unknown_kind() {
        let temp = tempfile::tempdir().unwrap();
        let provider = temp.path().join("provider.wasm");
        std::fs::write(&provider, b"provider").unwrap();

        let error = TrustedLocalGrants::parse(&[grant(&provider, "azure")], None).unwrap_err();

        assert!(error.to_string().contains("aws or github"));
    }

    #[test]
    fn grant_parser_rejects_missing_equals() {
        let error = TrustedLocalGrants::parse(&["/tmp/provider.wasm".into()], None).unwrap_err();

        assert!(error.to_string().contains("PATH=KIND"));
    }

    #[test]
    fn grant_parser_splits_on_last_equals() {
        let temp = tempfile::tempdir().unwrap();
        let provider = temp.path().join("provider=local.wasm");
        std::fs::write(&provider, b"provider").unwrap();

        let grants = TrustedLocalGrants::parse(&[grant(&provider, "aws")], None).unwrap();

        assert!(
            grants
                .matching(&std::fs::canonicalize(provider).unwrap())
                .is_some()
        );
    }

    #[test]
    fn notice_text_records_command_line_origin() {
        let temp = tempfile::tempdir().unwrap();
        let provider = temp.path().join("provider.wasm");
        std::fs::write(&provider, b"provider").unwrap();
        let canonical = std::fs::canonicalize(&provider).unwrap();
        let grants = TrustedLocalGrants::parse(&[grant(&provider, "aws")], None).unwrap();

        assert_eq!(
            grants.matching(&canonical).unwrap().notice(),
            format!(
                "carina: trusting local provider {} as aws (--trust-local-provider)",
                canonical.display()
            )
        );
    }

    #[test]
    fn notice_text_records_environment_origin() {
        let temp = tempfile::tempdir().unwrap();
        let provider = temp.path().join("provider.wasm");
        std::fs::write(&provider, b"provider").unwrap();
        let canonical = std::fs::canonicalize(&provider).unwrap();
        let value = grant(&provider, "github");
        let grants = TrustedLocalGrants::parse(&[], Some(&value)).unwrap();

        assert_eq!(
            grants.matching(&canonical).unwrap().notice(),
            format!(
                "carina: trusting local provider {} as github (CARINA_TRUST_LOCAL_PROVIDER)",
                canonical.display()
            )
        );
    }
}
