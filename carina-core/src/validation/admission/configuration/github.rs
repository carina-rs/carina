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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepositoryPath(String);
impl RepositoryPath {
    pub fn parse(value: &str) -> Result<Self, GitHubDestinationError> {
        if value
            .split('/')
            .any(|segment| matches!(segment, "" | "." | ".."))
        {
            return Err(GitHubDestinationError::Path);
        }
        Ok(Self(value.into()))
    }
    pub fn state_path(value: &str) -> Result<Self, GitHubDestinationError> {
        let path = Self::parse(value)?;
        if value.ends_with(".lock") {
            return Err(GitHubDestinationError::LockPath);
        }
        Ok(path)
    }
    pub fn lock_path(&self) -> Self {
        Self(format!("{}.lock", self.0))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
