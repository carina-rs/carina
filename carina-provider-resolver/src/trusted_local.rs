use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io;
use std::path::PathBuf;

use carina_core::paths::{CanonicalPath, DisplayPath};

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

#[derive(Debug, Clone)]
pub(crate) struct TrustedLocalGrant {
    canonical_path: CanonicalPath,
    display_path: DisplayPath,
    class: ProviderTrustClass,
    origin: GrantOrigin,
}

impl PartialEq for TrustedLocalGrant {
    fn eq(&self, other: &Self) -> bool {
        self.canonical_path == other.canonical_path
            && self.class == other.class
            && self.origin == other.origin
    }
}

impl Eq for TrustedLocalGrant {}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrustedLocalGrants(Vec<TrustedLocalGrant>);

impl TrustedLocalGrants {
    pub fn parse(cli_values: &[String], env_value: Option<&str>) -> Result<Self, GrantParseError> {
        Self::parse_os(cli_values, env_value.map(OsStr::new))
    }

    pub fn parse_os(
        cli_values: &[String],
        env_value: Option<&OsStr>,
    ) -> Result<Self, GrantParseError> {
        let mut grants = Vec::new();
        for value in cli_values {
            insert_grant(&mut grants, parse_grant(value, GrantOrigin::CommandLine)?)?;
        }
        if let Some(env_value) = env_value.filter(|value| !value.is_empty()) {
            for value in std::env::split_paths(env_value) {
                let value = value
                    .into_os_string()
                    .into_string()
                    .map_err(|value| GrantParseError::NonUtf8Env { value })?;
                insert_grant(&mut grants, parse_grant(&value, GrantOrigin::Environment)?)?;
            }
        }
        Ok(Self(grants))
    }

    pub(crate) fn matching(&self, canonical_path: &CanonicalPath) -> Option<&TrustedLocalGrant> {
        self.0
            .iter()
            .find(|grant| &grant.canonical_path == canonical_path)
    }
}

impl TrustedLocalGrant {
    pub(crate) fn trust(&self) -> ProviderTrust {
        ProviderTrust::trusted_local(self.class)
    }

    pub(crate) fn notice(&self) -> String {
        format!(
            "carina: trusting local provider {} as {} ({})",
            self.display_path,
            trust_class_name(self.class),
            self.origin.label(),
        )
    }
}

#[derive(Debug)]
pub enum GrantParseError {
    MissingSeparator {
        value: String,
    },
    RelativePath {
        value: String,
    },
    NotWasm {
        value: String,
    },
    Absolutize {
        value: String,
        path: PathBuf,
        source: io::Error,
    },
    Canonicalize {
        value: String,
        path: PathBuf,
        source: io::Error,
    },
    NotAFile {
        value: String,
        path: DisplayPath,
    },
    UnknownKind {
        value: String,
    },
    NonUtf8Env {
        value: OsString,
    },
    ConflictingGrant {
        path: DisplayPath,
        first_kind: ProviderTrustClass,
        second_kind: ProviderTrustClass,
    },
}

impl fmt::Display for GrantParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingSeparator { value } => write!(
                formatter,
                "invalid trusted local provider grant {value:?}: expected PATH=KIND"
            ),
            Self::RelativePath { value } => write!(
                formatter,
                "invalid trusted local provider grant {value:?}: provider path must be absolute"
            ),
            Self::NotWasm { value } => write!(
                formatter,
                "invalid trusted local provider grant {value:?}: provider path must name a .wasm file"
            ),
            Self::Absolutize { value, path, .. } => write!(
                formatter,
                "invalid trusted local provider grant {value:?}: failed to make {} absolute",
                path.display()
            ),
            Self::Canonicalize { value, path, .. } => write!(
                formatter,
                "invalid trusted local provider grant {value:?}: failed to canonicalize {}",
                path.display()
            ),
            Self::NotAFile { value, path } => write!(
                formatter,
                "invalid trusted local provider grant {value:?}: {} is not a file",
                path
            ),
            Self::UnknownKind { value } => write!(
                formatter,
                "invalid trusted local provider grant {value:?}: KIND must be aws or github"
            ),
            Self::NonUtf8Env { value } => write!(
                formatter,
                "invalid CARINA_TRUST_LOCAL_PROVIDER entry {value:?}: PATH=KIND must be valid UTF-8"
            ),
            Self::ConflictingGrant {
                path,
                first_kind,
                second_kind,
            } => write!(
                formatter,
                "conflicting trusted local provider grants for {}: KIND is both {} and {}",
                path,
                trust_class_name(*first_kind),
                trust_class_name(*second_kind)
            ),
        }
    }
}

impl std::error::Error for GrantParseError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Absolutize { source, .. } | Self::Canonicalize { source, .. } => Some(source),
            _ => None,
        }
    }
}

fn insert_grant(
    grants: &mut Vec<TrustedLocalGrant>,
    grant: TrustedLocalGrant,
) -> Result<(), GrantParseError> {
    if let Some(existing) = grants
        .iter()
        .find(|existing| existing.canonical_path == grant.canonical_path)
    {
        if existing.class == grant.class {
            return Ok(());
        }
        return Err(GrantParseError::ConflictingGrant {
            path: grant.display_path,
            first_kind: existing.class,
            second_kind: grant.class,
        });
    }
    grants.push(grant);
    Ok(())
}

fn parse_grant(value: &str, origin: GrantOrigin) -> Result<TrustedLocalGrant, GrantParseError> {
    let (path, kind) = value
        .rsplit_once('=')
        .ok_or_else(|| GrantParseError::MissingSeparator {
            value: value.to_string(),
        })?;
    let path = PathBuf::from(path);
    if !path.is_absolute() {
        return Err(GrantParseError::RelativePath {
            value: value.to_string(),
        });
    }
    if path.extension().is_none_or(|extension| extension != "wasm") {
        return Err(GrantParseError::NotWasm {
            value: value.to_string(),
        });
    }
    let display_path = DisplayPath::new(&path).map_err(|source| GrantParseError::Absolutize {
        value: value.to_string(),
        path: path.clone(),
        source,
    })?;
    let resolved_path = carina_core::paths::ResolvedPath::new(&path)
        .and_then(|file| {
            file.metadata()?;
            Ok(file)
        })
        .map_err(|source| GrantParseError::Canonicalize {
            value: value.to_string(),
            path: path.clone(),
            source,
        })?;
    let canonical_path = resolved_path.identity();
    if !resolved_path.is_file() {
        return Err(GrantParseError::NotAFile {
            value: value.to_string(),
            path: display_path,
        });
    }
    let class = match kind {
        "aws" => ProviderTrustClass::Aws,
        "github" => ProviderTrustClass::GitHub,
        _ => {
            return Err(GrantParseError::UnknownKind {
                value: value.to_string(),
            });
        }
    };
    Ok(TrustedLocalGrant {
        canonical_path,
        display_path,
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
    use std::path::Path;

    #[test]
    fn grant_absolutize_error_retains_its_distinct_operation_and_source() {
        let error = GrantParseError::Absolutize {
            value: "provider.wasm=aws".into(),
            path: PathBuf::from("provider.wasm"),
            source: io::Error::new(io::ErrorKind::NotFound, "current directory unavailable"),
        };
        let message = error.to_string();
        assert!(message.contains("absolute"), "{message}");
        assert!(!message.contains("canonicalize"), "{message}");
        assert!(std::error::Error::source(&error).is_some());
    }

    #[cfg(unix)]
    #[test]
    fn grant_notice_and_conflict_preserve_symlink_path() {
        let temp = tempfile::tempdir().unwrap();
        let real = temp.path().join("real");
        let link = temp.path().join("link");
        std::fs::create_dir(&real).unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let provider = link.join("provider.wasm");
        std::fs::write(&provider, b"provider").unwrap();
        let grants = TrustedLocalGrants::parse(&[grant(&provider, "aws")], None).unwrap();
        let notice = grants.0[0].notice();
        assert!(notice.contains(&provider.display().to_string()), "{notice}");
        assert!(!notice.contains(&real.display().to_string()), "{notice}");

        let error = TrustedLocalGrants::parse(
            &[
                grant(&real.join("provider.wasm"), "aws"),
                grant(&provider, "github"),
            ],
            None,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains(&provider.display().to_string()), "{error}");
        assert!(!error.contains(&real.display().to_string()), "{error}");
    }

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

        let aws_path = CanonicalPath::new(&aws).unwrap();
        let github_path = CanonicalPath::new(&github).unwrap();
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

        assert!(matches!(&error, GrantParseError::RelativePath { .. }));
        assert!(error.to_string().contains("absolute"));
    }

    #[test]
    fn canonicalize_error_preserves_io_error_source() {
        let temp = tempfile::tempdir().unwrap();
        let missing = temp.path().join("missing.wasm");

        let error = TrustedLocalGrants::parse(&[grant(&missing, "aws")], None).unwrap_err();

        assert!(matches!(&error, GrantParseError::Canonicalize { .. }));
        assert!(
            std::error::Error::source(&error)
                .and_then(|source| source.downcast_ref::<io::Error>())
                .is_some()
        );
    }

    #[cfg(unix)]
    #[test]
    fn grant_parser_rejects_non_utf8_environment_entry() {
        use std::os::unix::ffi::OsStringExt;

        let value = OsString::from_vec(b"/tmp/provider.wasm=aws\xff".to_vec());

        let error = TrustedLocalGrants::parse_os(&[], Some(&value)).unwrap_err();

        assert!(matches!(error, GrantParseError::NonUtf8Env { .. }));
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
                .matching(&CanonicalPath::new(provider).unwrap())
                .is_some()
        );
    }

    #[test]
    fn grant_parser_rejects_conflicting_canonical_path_grants() {
        let temp = tempfile::tempdir().unwrap();
        let provider = temp.path().join("provider.wasm");
        let nested = temp.path().join("nested");
        std::fs::write(&provider, b"provider").unwrap();
        std::fs::create_dir(&nested).unwrap();
        let alias = nested.join("..").join("provider.wasm");
        let env_value = grant(&alias, "github");

        let error =
            TrustedLocalGrants::parse(&[grant(&provider, "aws")], Some(&env_value)).unwrap_err();

        assert!(matches!(&error, GrantParseError::ConflictingGrant { .. }));
        assert!(error.to_string().contains("conflicting"));
        assert!(error.to_string().contains(&alias.display().to_string()));
    }

    #[test]
    fn grant_parser_deduplicates_identical_canonical_path_grants() {
        let temp = tempfile::tempdir().unwrap();
        let provider = temp.path().join("provider.wasm");
        let nested = temp.path().join("nested");
        std::fs::write(&provider, b"provider").unwrap();
        std::fs::create_dir(&nested).unwrap();
        let alias = nested.join("..").join("provider.wasm");
        let env_value = grant(&alias, "aws");

        let grants =
            TrustedLocalGrants::parse(&[grant(&provider, "aws")], Some(&env_value)).unwrap();

        assert_eq!(grants.0.len(), 1);
    }

    #[test]
    fn notice_text_records_command_line_origin() {
        let temp = tempfile::tempdir().unwrap();
        let provider = temp.path().join("provider.wasm");
        std::fs::write(&provider, b"provider").unwrap();
        let canonical = CanonicalPath::new(&provider).unwrap();
        let grants = TrustedLocalGrants::parse(&[grant(&provider, "aws")], None).unwrap();

        assert_eq!(
            grants.matching(&canonical).unwrap().notice(),
            format!(
                "carina: trusting local provider {} as aws (--trust-local-provider)",
                provider.display()
            )
        );
    }

    #[test]
    fn notice_text_records_environment_origin() {
        let temp = tempfile::tempdir().unwrap();
        let provider = temp.path().join("provider.wasm");
        std::fs::write(&provider, b"provider").unwrap();
        let canonical = CanonicalPath::new(&provider).unwrap();
        let value = grant(&provider, "github");
        let grants = TrustedLocalGrants::parse(&[], Some(&value)).unwrap();

        assert_eq!(
            grants.matching(&canonical).unwrap().notice(),
            format!(
                "carina: trusting local provider {} as github (CARINA_TRUST_LOCAL_PROVIDER)",
                provider.display()
            )
        );
    }
}
