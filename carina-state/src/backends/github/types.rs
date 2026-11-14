use super::GitHubError;

/// A blob identity is content-addressed; it is not a fencing generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BlobSha(String);
impl BlobSha {
    pub(crate) fn parse(value: &str) -> Result<Self, GitHubError> {
        validate_sha(value)?;
        Ok(Self(value.to_ascii_lowercase()))
    }
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

/// A commit identifies the snapshot used for paired metadata/raw reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CommitSha(String);
impl CommitSha {
    pub(crate) fn parse(value: &str) -> Result<Self, GitHubError> {
        validate_sha(value)?;
        Ok(Self(value.to_ascii_lowercase()))
    }
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}
fn validate_sha(value: &str) -> Result<(), GitHubError> {
    if value.len() != 40 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(GitHubError::MalformedResponse);
    }
    Ok(())
}
