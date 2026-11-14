//! Validated GitHub destination components, shared by admission and URL adapters.
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum GitHubDestinationError {
    #[error("GitHub repository must be an owner/repo slug")]
    Repository,
    #[error("GitHub branch must not be empty")]
    Branch,
    #[error("GitHub path must be relative, non-empty, and contain no empty, '.' or '..' segments")]
    Path,
    #[error("GitHub state path must not end in .lock")]
    LockPath,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepositorySlug(String);
impl RepositorySlug {
    pub fn parse(value: &str) -> Result<Self, GitHubDestinationError> {
        let parts: Vec<_> = value.split('/').collect();
        if parts.len() != 2
            || parts.iter().any(|part| {
                part.is_empty()
                    || matches!(*part, "." | "..")
                    || !part
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
            })
        {
            return Err(GitHubDestinationError::Repository);
        }
        Ok(Self(value.into()))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitBranch(String);
impl GitBranch {
    pub fn parse(value: &str) -> Result<Self, GitHubDestinationError> {
        if value.is_empty() {
            return Err(GitHubDestinationError::Branch);
        }
        Ok(Self(value.into()))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A state file path, validated independently of its derived lease path.
///
/// ```compile_fail
/// use carina_core::validation::admission::configuration::backend::StatePath;
/// let state: StatePath = StatePath::parse("state.json").unwrap().lock_path();
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatePath(String);
impl StatePath {
    pub fn parse(value: &str) -> Result<Self, GitHubDestinationError> {
        if value
            .split('/')
            .any(|segment| matches!(segment, "" | "." | ".."))
        {
            return Err(GitHubDestinationError::Path);
        }
        if value.ends_with(".lock") {
            return Err(GitHubDestinationError::LockPath);
        }
        Ok(Self(value.into()))
    }
    pub fn lock_path(&self) -> LeasePath {
        LeasePath(format!("{}.lock", self.0))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Minted only from a validated state path; never accepted as a state path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeasePath(String);
impl LeasePath {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Credentials are deliberately absent from persistent backend settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitHubBackendSettings {
    repository: RepositorySlug,
    branch: GitBranch,
    path: StatePath,
}
impl GitHubBackendSettings {
    /// Assemble already validated destination components, including URL parts.
    pub fn from_parts(repository: RepositorySlug, branch: GitBranch, path: StatePath) -> Self {
        Self {
            repository,
            branch,
            path,
        }
    }
    pub fn repository(&self) -> &RepositorySlug {
        &self.repository
    }
    pub fn branch(&self) -> &GitBranch {
        &self.branch
    }
    pub fn path(&self) -> &StatePath {
        &self.path
    }
}

/// The declaration field, rather than the authored key, chooses its validator.
#[derive(Default)]
pub(super) struct GitHubSettingsBuilder {
    repository: Option<RepositorySlug>,
    branch: Option<GitBranch>,
    path: Option<StatePath>,
}
impl GitHubSettingsBuilder {
    pub(super) fn place(&mut self, field: &str, value: &str) -> Result<(), GitHubDestinationError> {
        match field {
            "repository" => self.repository = Some(RepositorySlug::parse(value)?),
            "branch" => self.branch = Some(GitBranch::parse(value)?),
            "path" => self.path = Some(StatePath::parse(value)?),
            _ => unreachable!("declared GitHub field"),
        }
        Ok(())
    }
    pub(super) fn finish(self) -> Result<GitHubBackendSettings, super::BackendConfigErrors> {
        use super::{BackendAttributeError, BackendConfigErrors};
        let mut missing = Vec::new();
        for (field, absent) in [
            ("repository", self.repository.is_none()),
            ("branch", self.branch.is_none()),
            ("path", self.path.is_none()),
        ] {
            if absent {
                missing.push(BackendAttributeError::MissingRequiredAttribute {
                    backend_type: "github".into(),
                    attribute: field.into(),
                });
            }
        }
        match (self.repository, self.branch, self.path) {
            (Some(repository), Some(branch), Some(path)) => {
                Ok(GitHubBackendSettings::from_parts(repository, branch, path))
            }
            _ => Err(BackendConfigErrors::new(missing)),
        }
    }
}
