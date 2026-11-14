//! GitHub.com Contents storage and SHA-conditioned leases.
use async_trait::async_trait;
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use thiserror::Error;
use tokio::time::Instant;

use super::url::encode_component;
use crate::{BackendError, LeasePath, StatePath, StorageUnavailable};
use crate::{
    BackendResult, GitHubBackendSettings, LoadedState, LockInfo, RawState, StateBackend, StateFile,
    StorageReadiness,
};

mod transport;
mod types;
use transport::ReqwestTransport;
pub use types::{BlobSha, CommitSha};

pub const MAX_BYTES: usize = 10_000_000;
pub const RAW_MEDIA_TYPE: &str = "application/vnd.github.raw+json";
pub const JSON_MEDIA_TYPE: &str = "application/vnd.github+json";

/// Terminal protocol failures. Response bodies and credentials are never
/// included in diagnostics, including malformed and oversized responses.
#[derive(Debug, Error)]
pub enum GitHubError {
    #[error("GitHub credentials unavailable: {0}")]
    Credentials(&'static str),
    #[error("GitHub credentials unavailable: {source}")]
    CredentialsUnavailable {
        #[source]
        source: carina_core::github_auth::GitHubTokenError,
    },
    #[error("GitHub credential lookup exceeded its deadline: {source}")]
    CredentialsDeadline {
        #[source]
        source: carina_core::github_auth::GitHubTokenError,
    },
    #[error("GitHub credential lookup task failed: {source}")]
    CredentialTask {
        #[source]
        source: tokio::task::JoinError,
    },
    #[error("GitHub transport failure ({kind})")]
    Transport {
        kind: &'static str,
        #[source]
        source: reqwest::Error,
    },
    #[error("GitHub operation deadline exceeded; a dispatched mutation may have completed")]
    TransportDeadline {
        #[source]
        source: reqwest::Error,
    },
    #[error("GitHub API returned HTTP {status}: {body} (request ID: {request_id:?})")]
    Api {
        status: u16,
        body: String,
        request_id: Option<String>,
    },
    #[error(
        "GitHub repository or branch is missing or unusable; create it before using this backend"
    )]
    Unavailable,
    #[error("Malformed GitHub response")]
    MalformedResponse,
    #[error("GitHub state path refers to a non-file object")]
    NonFile,
    #[error("Corrupt GitHub lease; inspect and repair the lock file before continuing")]
    CorruptLease,
    #[error("Unexpected GitHub mutation outcome: {reason}")]
    UnexpectedOutcome { reason: &'static str },
    #[error("GitHub branch contention persisted after {attempts} attempts")]
    Contention { attempts: usize },
    #[error("GitHub rate limit (HTTP {status}); retry hint: {retry_after:?}")]
    RateLimited {
        status: u16,
        retry_after: Option<Duration>,
        request_id: Option<String>,
    },
    #[error("GitHub state size {size} decoded bytes exceeds the cap of {cap} decoded bytes (base64 content: {encoded} bytes, plus request metadata)", encoded = .size.div_ceil(3).saturating_mul(4))]
    TooLarge { size: usize, cap: usize },
    #[error("GitHub operation deadline exceeded; a dispatched mutation may have completed")]
    Deadline,
}

impl From<carina_core::github_auth::GitHubTokenError> for GitHubError {
    fn from(source: carina_core::github_auth::GitHubTokenError) -> Self {
        use carina_core::github_auth::GitHubTokenError;
        match source {
            GitHubTokenError::Deadline => Self::CredentialsDeadline { source },
            GitHubTokenError::Unavailable => Self::CredentialsUnavailable { source },
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    Get,
    Put,
    Delete,
}

/// HTTP boundary only. Domain addresses remain validated types until this
/// request is serialized. Deliberately has no Debug implementation.
#[derive(Clone)]
pub struct GitHubRequest {
    pub method: Method,
    pub path: String,
    pub reference: Option<String>,
    pub accept: &'static str,
    pub body: Option<Value>,
}

pub struct GitHubResponse {
    pub status: u16,
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
}

/// Injectable, bounded transport. Implementations must cap streamed response
/// bodies at MAX_BYTES, sanitize reflected credentials in error responses,
/// and respect the caller's absolute deadline.
#[async_trait]
pub trait GitHubTransport: Send + Sync {
    async fn execute(
        &self,
        request: GitHubRequest,
        deadline: Instant,
    ) -> Result<GitHubResponse, GitHubError>;
}

pub struct GitHubBackend {
    settings: GitHubBackendSettings,
    transport: Arc<dyn GitHubTransport>,
    mutations: tokio::sync::Mutex<()>,
    // None = not yet observed; Some(None) = known absent. Raw export reads
    // deliberately never update this writer's precondition.
    expected_state: Mutex<Option<Option<BlobSha>>>,
    migration_logged: OnceLock<crate::MigrationInfo>,
}

const OPERATION_BUDGET: Duration = Duration::from_secs(30);
// The existing finalizer allows 1s to flush and 2s overall. Each complete
// cleanup operation, including its gate, retries and read-back, is bounded to 900ms.
const CLEANUP_BUDGET: Duration = Duration::from_millis(900);
const MAX_ATTEMPTS: usize = 5;

struct Operation {
    deadline: Instant,
    cleanup: bool,
    rate_retries: AtomicUsize,
}
impl Operation {
    fn new(cleanup: bool) -> Self {
        Self {
            deadline: Instant::now()
                + if cleanup {
                    CLEANUP_BUDGET
                } else {
                    OPERATION_BUDGET
                },
            cleanup,
            rate_retries: AtomicUsize::new(0),
        }
    }
}

/// State and lease addresses remain distinct until HTTP serialization.
#[derive(Clone, Copy)]
enum ContentsPath<'a> {
    State(&'a StatePath),
    Lease(&'a LeasePath),
}
impl<'a> ContentsPath<'a> {
    fn as_str(self) -> &'a str {
        match self {
            Self::State(path) => path.as_str(),
            Self::Lease(path) => path.as_str(),
        }
    }
}
impl<'a> From<&'a StatePath> for ContentsPath<'a> {
    fn from(path: &'a StatePath) -> Self {
        Self::State(path)
    }
}
impl<'a> From<&'a LeasePath> for ContentsPath<'a> {
    fn from(path: &'a LeasePath) -> Self {
        Self::Lease(path)
    }
}

struct FileMetadata {
    sha: BlobSha,
    size: usize,
    inline: Option<String>,
}
struct ObservedFile {
    sha: BlobSha,
    bytes: Vec<u8>,
}
enum Purpose<'a> {
    State(WriteLease<'a>),
    Acquire(&'a LockInfo),
    Owned(&'a str),
}

enum WriteLease<'a> {
    Unlocked,
    Held(&'a LockInfo),
}

impl GitHubBackend {
    pub(crate) fn with_transport(
        settings: GitHubBackendSettings,
        transport: Arc<dyn GitHubTransport>,
    ) -> Self {
        Self {
            settings,
            transport,
            mutations: tokio::sync::Mutex::new(()),
            expected_state: Mutex::new(None),
            migration_logged: OnceLock::new(),
        }
    }
    pub(crate) async fn from_config(settings: &GitHubBackendSettings) -> BackendResult<Self> {
        let token = tokio::task::spawn_blocking(carina_core::github_auth::get_github_token)
            .await
            .map_err(|source| GitHubError::CredentialTask { source })?
            .map_err(GitHubError::from)?;
        Ok(Self::with_transport(
            settings.clone(),
            Arc::new(ReqwestTransport::new(token)?),
        ))
    }

    fn repository_route(&self) -> String {
        format!("/repos/{}", self.settings.repository().as_str())
    }
    fn contents_route(&self, path: ContentsPath<'_>) -> String {
        format!(
            "{}/contents/{}",
            self.repository_route(),
            path.as_str()
                .split('/')
                .map(encode_component)
                .collect::<Vec<_>>()
                .join("/")
        )
    }
    fn location(&self) -> String {
        super::StateUrl::GitHub {
            repository: self.settings.repository().clone(),
            branch: self.settings.branch().clone(),
            path: self.settings.path().clone(),
        }
        .display_target()
    }
    async fn request(
        &self,
        request: GitHubRequest,
        operation: &Operation,
    ) -> BackendResult<GitHubResponse> {
        loop {
            let response = bounded(operation.deadline, async {
                self.transport
                    .execute(request.clone(), operation.deadline)
                    .await
                    .map_err(BackendError::from)
            })
            .await?;
            check_size(response.body.len())?;
            if matches!(response.status, 403 | 429) {
                let primary = response
                    .headers
                    .get("x-ratelimit-remaining")
                    .is_some_and(|value| value == "0");
                let secondary = serde_json::from_slice::<Value>(&response.body)
                    .ok()
                    .and_then(|value| {
                        value
                            .get("message")
                            .and_then(Value::as_str)
                            .map(str::to_lowercase)
                    })
                    .is_some_and(|message| message.contains("secondary rate limit"));
                if response.status == 429
                    || primary
                    || secondary
                    || response.headers.contains_key("retry-after")
                {
                    let hint = retry_hint(&response);
                    let retries = operation.rate_retries.load(Ordering::Relaxed);
                    if !operation.cleanup
                        && retries < 2
                        && let Some(hint) = hint
                    {
                        let delay = hint.max(Duration::from_millis(10 * (retries as u64 + 1)));
                        if delay < operation.deadline.saturating_duration_since(Instant::now()) {
                            operation.rate_retries.fetch_add(1, Ordering::Relaxed);
                            tokio::time::sleep(delay).await;
                            continue;
                        }
                    }
                    return Err(GitHubError::RateLimited {
                        status: response.status,
                        retry_after: hint.or_else(|| secondary.then_some(Duration::from_secs(60))),
                        request_id: request_id(&response),
                    }
                    .into());
                }
            }
            return Ok(response);
        }
    }
    fn api_error(response: &GitHubResponse) -> BackendError {
        GitHubError::Api {
            status: response.status,
            body: String::from_utf8_lossy(&response.body)
                .chars()
                .filter(|ch| !ch.is_control())
                .take(2048)
                .collect(),
            request_id: request_id(response),
        }
        .into()
    }
    async fn get(
        &self,
        path: String,
        reference: Option<&CommitSha>,
        accept: &'static str,
        operation: &Operation,
    ) -> BackendResult<GitHubResponse> {
        self.request(
            GitHubRequest {
                method: Method::Get,
                path,
                reference: reference.map(|sha| sha.as_str().into()),
                accept,
                body: None,
            },
            operation,
        )
        .await
    }
    async fn destination(&self, operation: &Operation) -> BackendResult<CommitSha> {
        let response = self
            .get(self.repository_route(), None, JSON_MEDIA_TYPE, operation)
            .await?;
        if response.status == 404 {
            return Err(GitHubError::Unavailable.into());
        }
        if response.status != 200 {
            return Err(Self::api_error(&response));
        }
        let repo = decode(&response.body)?;
        if repo.get("id").and_then(Value::as_u64).is_none() {
            return Err(GitHubError::MalformedResponse.into());
        }
        let mut unavailable = false;
        for flag in ["archived", "disabled"] {
            match repo.get(flag) {
                Some(Value::Bool(value)) => unavailable |= value,
                None => {}
                _ => return Err(GitHubError::MalformedResponse.into()),
            }
        }
        if unavailable {
            return Err(GitHubError::Unavailable.into());
        }
        self.branch_snapshot(operation).await
    }
    async fn branch_snapshot(&self, operation: &Operation) -> BackendResult<CommitSha> {
        // Reviewer measurement for #4436: branches/exp%2Fslash-branch and
        // branches/exp/slash-branch both returned 200, as did Contents with
        // ref=exp%2Fslash-branch. Preserve component encoding for slash branches.
        let response = self
            .get(
                format!(
                    "{}/branches/{}",
                    self.repository_route(),
                    encode_component(self.settings.branch().as_str())
                ),
                None,
                JSON_MEDIA_TYPE,
                operation,
            )
            .await?;
        if response.status == 404 {
            return Err(GitHubError::Unavailable.into());
        }
        if response.status != 200 {
            return Err(Self::api_error(&response));
        }
        let branch = decode(&response.body)?;
        Ok(CommitSha::parse(json_string(&branch["commit"], "sha")?)?)
    }
    async fn observe(
        &self,
        path: ContentsPath<'_>,
        operation: &Operation,
    ) -> BackendResult<Option<ObservedFile>> {
        // Access to the exact branch confirms the destination before a missing
        // path can mean absence. Metadata and bytes share one immutable ref.
        let commit = self.branch_snapshot(operation).await?;
        self.observe_at(path, &commit, operation).await
    }
    async fn metadata_at(
        &self,
        path: ContentsPath<'_>,
        commit: &CommitSha,
        operation: &Operation,
    ) -> BackendResult<Option<FileMetadata>> {
        let response = self
            .get(
                self.contents_route(path),
                Some(commit),
                JSON_MEDIA_TYPE,
                operation,
            )
            .await?;
        if response.status == 404 {
            return Ok(None);
        }
        if response.status != 200 {
            return Err(Self::api_error(&response));
        }
        let metadata = decode(&response.body)?;
        if metadata.is_array() || json_string(&metadata, "type")? != "file" {
            return Err(GitHubError::NonFile.into());
        }
        let sha = BlobSha::parse(json_string(&metadata, "sha")?)?;
        let size = metadata
            .get("size")
            .and_then(Value::as_u64)
            .ok_or(GitHubError::MalformedResponse)?;
        let size = usize::try_from(size).unwrap_or(usize::MAX);
        check_size(size)?;
        let inline = match json_string(&metadata, "encoding")? {
            "base64" => Some(json_string(&metadata, "content")?.to_owned()),
            "none" => None,
            _ => return Err(GitHubError::MalformedResponse.into()),
        };
        Ok(Some(FileMetadata { sha, size, inline }))
    }
    async fn observe_at(
        &self,
        path: ContentsPath<'_>,
        commit: &CommitSha,
        operation: &Operation,
    ) -> BackendResult<Option<ObservedFile>> {
        let Some(metadata) = self.metadata_at(path, commit, operation).await? else {
            return Ok(None);
        };
        let bytes = if let Some(inline) = metadata.inline {
            // GitHub wraps inline base64 with newlines.
            let encoded: Vec<_> = inline
                .bytes()
                .filter(|byte| !byte.is_ascii_whitespace())
                .collect();
            STANDARD
                .decode(encoded)
                .map_err(|_| GitHubError::MalformedResponse)?
        } else {
            let response = self
                .get(
                    self.contents_route(path),
                    Some(commit),
                    RAW_MEDIA_TYPE,
                    operation,
                )
                .await?;
            if response.status != 200 {
                return Err(Self::api_error(&response));
            }
            response.body
        };
        if bytes.len() != metadata.size {
            return Err(GitHubError::MalformedResponse.into());
        }
        Ok(Some(ObservedFile {
            sha: metadata.sha,
            bytes,
        }))
    }
    async fn lease(&self, operation: &Operation) -> BackendResult<Option<(LockInfo, BlobSha)>> {
        self.observe((&self.settings.path().lock_path()).into(), operation)
            .await?
            .map(|file| Ok((decode_lease(&file.bytes)?, file.sha)))
            .transpose()
    }
    async fn verify_owner(&self, lock: &LockInfo, operation: &Operation) -> BackendResult<()> {
        match self.lease(operation).await? {
            Some((current, _)) if current.id() == lock.id() && !current.is_expired() => Ok(()),
            _ => Err(BackendError::LockNotHeld(lock.id().into())),
        }
    }
    fn lost_race(
        purpose: &Purpose<'_>,
        current: Option<ObservedFile>,
        attempt: usize,
    ) -> BackendResult<BackendError> {
        Ok(match purpose {
            Purpose::State(_) => BackendError::StateWriteConflict { attempts: attempt },
            Purpose::Acquire(candidate) => match current {
                Some(file) => BackendError::locked(&decode_lease(&file.bytes)?),
                None => BackendError::LockNotHeld(candidate.id().into()),
            },
            Purpose::Owned(id) => {
                if let Some(file) = current {
                    decode_lease(&file.bytes)?;
                }
                BackendError::LockNotHeld((*id).into())
            }
        })
    }
    async fn conditional(
        &self,
        path: ContentsPath<'_>,
        bytes: Option<&[u8]>,
        expected: Option<&BlobSha>,
        purpose: Purpose<'_>,
        operation: &Operation,
    ) -> BackendResult<Option<BlobSha>> {
        if let Some(bytes) = bytes {
            check_size(bytes.len())?;
        }
        let message = match (&purpose, bytes.is_some()) {
            (Purpose::State(_), _) => "carina: update state",
            (_, true) => "carina: update lease",
            (_, false) => "carina: release lease",
        };
        let mut expected = expected.cloned();
        let mut body = json!({"message":message,"branch":self.settings.branch().as_str()});
        if let Some(sha) = &expected {
            body["sha"] = json!(sha.as_str());
        }
        if let Some(bytes) = bytes {
            body["content"] = json!(STANDARD.encode(bytes));
        }
        let mut request = GitHubRequest {
            method: if bytes.is_some() {
                Method::Put
            } else {
                Method::Delete
            },
            path: self.contents_route(path),
            reference: None,
            accept: JSON_MEDIA_TYPE,
            body: Some(body),
        };
        for attempt in 1..=MAX_ATTEMPTS {
            let lock_to_verify = match &purpose {
                Purpose::State(WriteLease::Held(lock)) => Some(*lock),
                _ => None,
            };
            if let Some(lock) = lock_to_verify {
                self.verify_owner(lock, operation).await?;
            }
            let response = self.request(request.clone(), operation).await?;
            if response.status == 409 {
                let current = self.observe(path, operation).await?;
                if matches!(purpose, Purpose::Acquire(_)) && expected.is_some() && current.is_none()
                {
                    // A conditional takeover lost to removal of the expired
                    // lease. Creating an absent path needs no replacement SHA;
                    // it still competes atomically with other acquirers.
                    expected = None;
                    request
                        .body
                        .as_mut()
                        .and_then(Value::as_object_mut)
                        .expect("mutation object")
                        .remove("sha");
                } else if current.as_ref().map(|file| &file.sha) != expected.as_ref() {
                    return Err(Self::lost_race(&purpose, current, attempt)?);
                }
                if attempt == MAX_ATTEMPTS {
                    return Err(GitHubError::Contention { attempts: attempt }.into());
                }
                // Branch contention keeps the same bytes and precondition;
                // only a vanished takeover target changes to absence above.
                // A new blob SHA with stale content would overwrite the winner.
                let jitter = (uuid::Uuid::new_v4().as_u128() % 25) as u64;
                let delay = Duration::from_millis(10 * attempt as u64 + jitter);
                if delay >= operation.deadline.saturating_duration_since(Instant::now()) {
                    return Err(GitHubError::Deadline.into());
                }
                tokio::time::sleep(delay).await;
                continue;
            }
            if !matches!(response.status, 200 | 201) {
                return Err(Self::api_error(&response));
            }
            let (commit, success) = (|| -> Result<(CommitSha, Option<BlobSha>), GitHubError> {
                let value: Value = serde_json::from_slice(&response.body)
                    .map_err(|_| GitHubError::MalformedResponse)?;
                let commit = CommitSha::parse(json_string(&value["commit"], "sha")?)?;
                if bytes.is_some() {
                    Ok((
                        commit,
                        Some(BlobSha::parse(json_string(&value["content"], "sha")?)?),
                    ))
                } else if value.get("content") == Some(&Value::Null) && response.status == 200 {
                    Ok((commit, None))
                } else {
                    Err(GitHubError::MalformedResponse)
                }
            })()
            .map_err(|_| GitHubError::UnexpectedOutcome {
                reason: "successful mutation returned malformed metadata",
            })?;
            // Prove what this acknowledgement published, even if another
            // client has already advanced the live branch (including release
            // followed immediately by a new acquisition).
            let observed = self.observe_at(path, &commit, operation).await?;
            let matches = match (bytes, success.as_ref(), observed.as_ref()) {
                (Some(bytes), Some(sha), Some(file)) => sha == &file.sha && bytes == file.bytes,
                (None, None, None) => true,
                _ => false,
            };
            if !matches {
                return Err(GitHubError::UnexpectedOutcome {reason:"acknowledged mutation differs from read-back; possible competing winners"}.into());
            }
            return Ok(success);
        }
        unreachable!("bounded conditional loop always returns")
    }
    fn state_bytes(state: &StateFile) -> BackendResult<Vec<u8>> {
        state
            .validate_unique_identities()
            .map_err(BackendError::InvalidState)?;
        let bytes = carina_core::utils::pretty_with_newline_bytes(state)
            .map_err(|error| BackendError::Serialization(error.to_string()))?;
        check_size(bytes.len())?;
        Ok(bytes)
    }
    async fn write(
        &self,
        bytes: &[u8],
        lease: WriteLease<'_>,
        operation: &Operation,
    ) -> BackendResult<()> {
        let cached = self
            .expected_state
            .lock()
            .expect("state revision mutex")
            .clone();
        let expected = match cached {
            Some(sha) => sha,
            None => {
                let commit = self.branch_snapshot(operation).await?;
                // Only the precondition is needed; never download a large
                // state's raw bytes just to learn its SHA during cleanup.
                let sha = self
                    .metadata_at(self.settings.path().into(), &commit, operation)
                    .await?
                    .map(|file| file.sha);
                *self.expected_state.lock().expect("state revision mutex") = Some(sha.clone());
                sha
            }
        };
        let sha = self
            .conditional(
                self.settings.path().into(),
                Some(bytes),
                expected.as_ref(),
                Purpose::State(lease),
                operation,
            )
            .await?;
        *self.expected_state.lock().expect("state revision mutex") = Some(sha);
        Ok(())
    }
    async fn renew(&self, lock: &LockInfo, operation: &Operation) -> BackendResult<LockInfo> {
        let (current, sha) = self
            .lease(operation)
            .await?
            .ok_or_else(|| BackendError::LockNotHeld(lock.id().into()))?;
        if current.id() != lock.id() || current.is_expired() {
            return Err(BackendError::LockNotHeld(lock.id().into()));
        }
        let renewed = lock.renewed();
        let bytes = serde_json::to_vec(&renewed).map_err(|_| GitHubError::CorruptLease)?;
        self.conditional(
            (&self.settings.path().lock_path()).into(),
            Some(&bytes),
            Some(&sha),
            Purpose::Owned(lock.id()),
            operation,
        )
        .await?;
        Ok(renewed)
    }
    async fn release(&self, id: &str, operation: &Operation) -> BackendResult<()> {
        let (current, sha) = self
            .lease(operation)
            .await?
            .ok_or_else(|| BackendError::LockNotFound(id.into()))?;
        if current.id() != id {
            return Err(BackendError::LockMismatch {
                expected: id.into(),
                actual: current.id().into(),
            });
        }
        self.conditional(
            (&self.settings.path().lock_path()).into(),
            None,
            Some(&sha),
            Purpose::Owned(id),
            operation,
        )
        .await?;
        Ok(())
    }
    async fn write_with_budget(
        &self,
        state: &StateFile,
        lock: Option<&LockInfo>,
        cleanup: bool,
    ) -> BackendResult<()> {
        let operation = Operation::new(cleanup);
        bounded(operation.deadline, async {
            let bytes = Self::state_bytes(state)?;
            let _gate = self.mutations.lock().await;
            // A renewal adds a mutation and read-back without fencing state
            // against lease takeover. Use the ordinary owner/expiry check on
            // every PUT attempt, within the unchanged cleanup deadline.
            let lease = lock.map_or(WriteLease::Unlocked, WriteLease::Held);
            self.write(&bytes, lease, &operation).await
        })
        .await
    }
    async fn release_with_budget(&self, id: &str, cleanup: bool) -> BackendResult<()> {
        let operation = Operation::new(cleanup);
        bounded(operation.deadline, async {
            let _gate = self.mutations.lock().await;
            self.release(id, &operation).await
        })
        .await
    }
}

fn decode(bytes: &[u8]) -> Result<Value, GitHubError> {
    serde_json::from_slice(bytes).map_err(|_| GitHubError::MalformedResponse)
}
fn json_string<'a>(value: &'a Value, key: &str) -> Result<&'a str, GitHubError> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or(GitHubError::MalformedResponse)
}
fn check_size(size: usize) -> Result<(), GitHubError> {
    if size > MAX_BYTES {
        Err(GitHubError::TooLarge {
            size,
            cap: MAX_BYTES,
        })
    } else {
        Ok(())
    }
}
fn decode_lease(bytes: &[u8]) -> Result<LockInfo, GitHubError> {
    let lock = LockInfo::from_json_slice(bytes).map_err(|_| GitHubError::CorruptLease)?;
    uuid::Uuid::parse_str(lock.id()).map_err(|_| GitHubError::CorruptLease)?;
    Ok(lock)
}
fn request_id(response: &GitHubResponse) -> Option<String> {
    response.headers.get("x-github-request-id").cloned()
}
fn retry_hint(response: &GitHubResponse) -> Option<Duration> {
    if let Some(value) = response.headers.get("retry-after") {
        if let Ok(seconds) = value.parse::<u64>() {
            return Some(Duration::from_secs(seconds));
        }
        if let Ok(date) = chrono::DateTime::parse_from_rfc2822(value) {
            return Some(Duration::from_secs(
                u64::try_from(
                    date.timestamp()
                        .saturating_sub(chrono::Utc::now().timestamp()),
                )
                .unwrap_or(0),
            ));
        }
    }
    let reset = response
        .headers
        .get("x-ratelimit-reset")?
        .parse::<i64>()
        .ok()?;
    Some(Duration::from_secs(
        u64::try_from(reset.saturating_sub(chrono::Utc::now().timestamp())).unwrap_or(0),
    ))
}
async fn bounded<T>(
    deadline: Instant,
    operation: impl std::future::Future<Output = BackendResult<T>>,
) -> BackendResult<T> {
    tokio::time::timeout_at(deadline, operation)
        .await
        .map_err(|_| GitHubError::Deadline)?
}

#[async_trait]
impl StateBackend for GitHubBackend {
    async fn read_state(&self) -> BackendResult<Option<LoadedState>> {
        let operation = Operation::new(false);
        bounded(operation.deadline, async {
            let _gate = self.mutations.lock().await;
            let Some(file) = self
                .observe(self.settings.path().into(), &operation)
                .await?
            else {
                *self.expected_state.lock().expect("state revision mutex") = Some(None);
                return Ok(None);
            };
            let (state, migration) = crate::check_and_migrate_bytes(&file.bytes)
                .map_err(|error| error.with_state_read_remote_location(&self.location()))?
                .into_parts();
            *self.expected_state.lock().expect("state revision mutex") = Some(Some(file.sha));
            Ok(Some(if let Some(info) = migration {
                crate::log_state_migration_once(&self.migration_logged, info, &self.location());
                LoadedState::Migrated { state, info }
            } else {
                LoadedState::Pristine(state)
            }))
        })
        .await
    }
    async fn read_state_bytes(&self) -> BackendResult<Option<RawState>> {
        let operation = Operation::new(false);
        bounded(operation.deadline, async {
            Ok(self
                .observe(self.settings.path().into(), &operation)
                .await?
                .map(|file| RawState::remote(file.bytes, self.location())))
        })
        .await
    }
    async fn write_state(&self, state: &StateFile) -> BackendResult<()> {
        self.write_with_budget(state, None, false).await
    }
    async fn acquire_lock(&self, operation_name: &str) -> BackendResult<LockInfo> {
        let operation = Operation::new(false);
        bounded(operation.deadline, async {
            let _gate = self.mutations.lock().await;
            let expected = match self.lease(&operation).await? {
                Some((current, _)) if !current.is_expired() => {
                    return Err(BackendError::locked(&current));
                }
                Some((_, sha)) => Some(sha),
                None => None,
            };
            let lock = LockInfo::new(operation_name);
            let bytes = serde_json::to_vec(&lock).map_err(|_| GitHubError::CorruptLease)?;
            self.conditional(
                (&self.settings.path().lock_path()).into(),
                Some(&bytes),
                expected.as_ref(),
                Purpose::Acquire(&lock),
                &operation,
            )
            .await?;
            Ok(lock)
        })
        .await
    }
    async fn release_lock(&self, lock: &LockInfo) -> BackendResult<()> {
        self.release_with_budget(lock.id(), false).await
    }
    async fn renew_lock(&self, lock: &LockInfo) -> BackendResult<LockInfo> {
        let operation = Operation::new(false);
        bounded(operation.deadline, async {
            let _gate = self.mutations.lock().await;
            self.renew(lock, &operation).await
        })
        .await
    }
    async fn write_state_locked(&self, state: &StateFile, lock: &LockInfo) -> BackendResult<()> {
        self.write_with_budget(state, Some(lock), false).await
    }
    async fn write_state_for_cleanup(&self, state: &StateFile) -> BackendResult<()> {
        self.write_with_budget(state, None, true).await
    }
    async fn write_state_locked_for_cleanup(
        &self,
        state: &StateFile,
        lock: &LockInfo,
    ) -> BackendResult<()> {
        self.write_with_budget(state, Some(lock), true).await
    }
    async fn release_lock_for_cleanup(&self, lock: &LockInfo) -> BackendResult<()> {
        self.release_with_budget(lock.id(), true).await
    }
    async fn force_unlock(&self, id: &str) -> BackendResult<()> {
        self.release_with_budget(id, false).await
    }
    async fn storage_readiness(&self) -> BackendResult<StorageReadiness> {
        let operation = Operation::new(false);
        bounded(operation.deadline, async {
            match self.destination(&operation).await {
                Ok(_) => Ok(StorageReadiness::Ready),
                Err(error @ BackendError::GitHub(GitHubError::Unavailable)) => Ok(
                    StorageReadiness::Unavailable(StorageUnavailable::new(error)),
                ),
                Err(error) => Err(error),
            }
        })
        .await
    }
}
