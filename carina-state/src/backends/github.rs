//! GitHub.com Contents storage and SHA-conditioned leases.
use async_trait::async_trait;
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};
use thiserror::Error;
use tokio::time::Instant;

use super::url::encode_component;
use crate::{BackendError, RepositoryPath, StorageUnavailable};
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
    #[error("GitHub transport failure ({kind})")]
    Transport { kind: &'static str },
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
// cleanup operation, including its gate, retries and read-back, fits in 900ms.
const CLEANUP_BUDGET: Duration = Duration::from_millis(900);
const MAX_ATTEMPTS: usize = 5;

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
    // Cleanup has just renewed and read back this lease under the mutation
    // gate. As in S3 cleanup, do not repeat the first ownership read; retries
    // must still recheck it. This does not add cross-path fencing.
    Renewed(&'a LockInfo),
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
            .map_err(|_| GitHubError::Credentials("token lookup failed"))?
            .map_err(|_| {
                GitHubError::Credentials(
                    "set GITHUB_TOKEN or run gh auth login; token lookup is bounded to 5s",
                )
            })?;
        Ok(Self::with_transport(
            settings.clone(),
            Arc::new(ReqwestTransport::new(token)?),
        ))
    }

    fn repository_route(&self) -> String {
        format!("/repos/{}", self.settings.repository().as_str())
    }
    fn contents_route(&self, path: &RepositoryPath) -> String {
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
        deadline: Instant,
    ) -> BackendResult<GitHubResponse> {
        let response = bounded(deadline, async {
            self.transport
                .execute(request, deadline)
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
                let retry_after =
                    retry_hint(&response).or_else(|| secondary.then_some(Duration::from_secs(60)));
                return Err(GitHubError::RateLimited {
                    status: response.status,
                    retry_after,
                    request_id: request_id(&response),
                }
                .into());
            }
        }
        Ok(response)
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
        deadline: Instant,
    ) -> BackendResult<GitHubResponse> {
        self.request(
            GitHubRequest {
                method: Method::Get,
                path,
                reference: reference.map(|sha| sha.as_str().into()),
                accept,
                body: None,
            },
            deadline,
        )
        .await
    }
    async fn destination(&self, deadline: Instant) -> BackendResult<CommitSha> {
        let response = self
            .get(self.repository_route(), None, JSON_MEDIA_TYPE, deadline)
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
        self.branch_snapshot(deadline).await
    }
    async fn branch_snapshot(&self, deadline: Instant) -> BackendResult<CommitSha> {
        let response = self
            .get(
                format!(
                    "{}/branches/{}",
                    self.repository_route(),
                    encode_component(self.settings.branch().as_str())
                ),
                None,
                JSON_MEDIA_TYPE,
                deadline,
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
        path: &RepositoryPath,
        deadline: Instant,
    ) -> BackendResult<Option<ObservedFile>> {
        // Resolve the explicit branch through the branches endpoint, never a
        // default branch or a Git Data transaction. Both Contents reads use
        // this same immutable commit, so metadata and bytes cannot disagree
        // merely because another path advanced the branch.
        // Access to this exact branch also confirms access to its repository.
        // Avoid a redundant repository metadata request for every lease read.
        let commit = self.branch_snapshot(deadline).await?;
        let route = self.contents_route(path);
        let response = self
            .get(route.clone(), Some(&commit), JSON_MEDIA_TYPE, deadline)
            .await?;
        if response.status == 404 {
            return Ok(None);
        }
        if response.status != 200 {
            return Err(Self::api_error(&response));
        }
        let metadata = decode(&response.body)?;
        if metadata.is_array() {
            return Err(GitHubError::NonFile.into());
        }
        if json_string(&metadata, "type")? != "file" {
            return Err(GitHubError::NonFile.into());
        }
        let sha = BlobSha::parse(json_string(&metadata, "sha")?)?;
        let size = metadata
            .get("size")
            .and_then(Value::as_u64)
            .ok_or(GitHubError::MalformedResponse)?;
        check_size(usize::try_from(size).unwrap_or(usize::MAX))?;
        let response = self
            .get(route, Some(&commit), RAW_MEDIA_TYPE, deadline)
            .await?;
        if response.status != 200 {
            return Err(Self::api_error(&response));
        }
        if response.body.len() as u64 != size {
            return Err(GitHubError::MalformedResponse.into());
        }
        Ok(Some(ObservedFile {
            sha,
            bytes: response.body,
        }))
    }
    async fn lease(&self, deadline: Instant) -> BackendResult<Option<(LockInfo, BlobSha)>> {
        self.observe(&self.settings.path().lock_path(), deadline)
            .await?
            .map(|file| Ok((decode_lease(&file.bytes)?, file.sha)))
            .transpose()
    }
    async fn verify_owner(&self, lock: &LockInfo, deadline: Instant) -> BackendResult<()> {
        match self.lease(deadline).await? {
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
        path: &RepositoryPath,
        bytes: Option<&[u8]>,
        expected: Option<&BlobSha>,
        purpose: Purpose<'_>,
        deadline: Instant,
    ) -> BackendResult<Option<BlobSha>> {
        if let Some(bytes) = bytes {
            check_size(bytes.len())?;
        }
        let message = match (&purpose, bytes.is_some()) {
            (Purpose::State(_), _) => "carina: update state",
            (_, true) => "carina: update lease",
            (_, false) => "carina: release lease",
        };
        let mut body = json!({"message":message,"branch":self.settings.branch().as_str()});
        if let Some(sha) = expected {
            body["sha"] = json!(sha.as_str());
        }
        if let Some(bytes) = bytes {
            body["content"] = json!(STANDARD.encode(bytes));
        }
        let request = GitHubRequest {
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
                Purpose::State(WriteLease::Renewed(lock)) if attempt > 1 => Some(*lock),
                _ => None,
            };
            if let Some(lock) = lock_to_verify {
                self.verify_owner(lock, deadline).await?;
            }
            let response = self.request(request.clone(), deadline).await?;
            if response.status == 409 {
                let current = self.observe(path, deadline).await?;
                if current.as_ref().map(|file| &file.sha) != expected {
                    return Err(Self::lost_race(&purpose, current, attempt)?);
                }
                if attempt == MAX_ATTEMPTS {
                    return Err(GitHubError::Contention { attempts: attempt }.into());
                }
                // Keep the exact same bytes and precondition. A new SHA with
                // stale content would overwrite the winner.
                let jitter = (uuid::Uuid::new_v4().as_u128() % 25) as u64;
                let delay = Duration::from_millis(10 * attempt as u64 + jitter);
                if Instant::now() + delay >= deadline {
                    return Err(GitHubError::Deadline.into());
                }
                tokio::time::sleep(delay).await;
                continue;
            }
            if !matches!(response.status, 200 | 201) {
                return Err(Self::api_error(&response));
            }
            let success = (|| -> Result<Option<BlobSha>, GitHubError> {
                let value: Value = serde_json::from_slice(&response.body)
                    .map_err(|_| GitHubError::MalformedResponse)?;
                let _commit = CommitSha::parse(json_string(&value["commit"], "sha")?)?;
                if bytes.is_some() {
                    Ok(Some(BlobSha::parse(json_string(
                        &value["content"],
                        "sha",
                    )?)?))
                } else if value.get("content") == Some(&Value::Null) && response.status == 200 {
                    Ok(None)
                } else {
                    Err(GitHubError::MalformedResponse)
                }
            })()
            .map_err(|_| GitHubError::UnexpectedOutcome {
                reason: "successful mutation returned malformed metadata",
            })?;
            let observed = self.observe(path, deadline).await?;
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
        deadline: Instant,
    ) -> BackendResult<()> {
        let cached = self
            .expected_state
            .lock()
            .expect("state revision mutex")
            .clone();
        let expected = match cached {
            Some(sha) => sha,
            None => {
                let sha = self
                    .observe(self.settings.path(), deadline)
                    .await?
                    .map(|file| file.sha);
                *self.expected_state.lock().expect("state revision mutex") = Some(sha.clone());
                sha
            }
        };
        let sha = self
            .conditional(
                self.settings.path(),
                Some(bytes),
                expected.as_ref(),
                Purpose::State(lease),
                deadline,
            )
            .await?;
        *self.expected_state.lock().expect("state revision mutex") = Some(sha);
        Ok(())
    }
    async fn renew(&self, lock: &LockInfo, deadline: Instant) -> BackendResult<LockInfo> {
        let (current, sha) = self
            .lease(deadline)
            .await?
            .ok_or_else(|| BackendError::LockNotHeld(lock.id().into()))?;
        if current.id() != lock.id() || current.is_expired() {
            return Err(BackendError::LockNotHeld(lock.id().into()));
        }
        let renewed = lock.renewed();
        let bytes = serde_json::to_vec(&renewed).map_err(|_| GitHubError::CorruptLease)?;
        self.conditional(
            &self.settings.path().lock_path(),
            Some(&bytes),
            Some(&sha),
            Purpose::Owned(lock.id()),
            deadline,
        )
        .await?;
        Ok(renewed)
    }
    async fn release(&self, id: &str, deadline: Instant) -> BackendResult<()> {
        let (current, sha) = self
            .lease(deadline)
            .await?
            .ok_or_else(|| BackendError::LockNotFound(id.into()))?;
        if current.id() != id {
            return Err(BackendError::LockMismatch {
                expected: id.into(),
                actual: current.id().into(),
            });
        }
        self.conditional(
            &self.settings.path().lock_path(),
            None,
            Some(&sha),
            Purpose::Owned(id),
            deadline,
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
        let deadline = Instant::now()
            + if cleanup {
                CLEANUP_BUDGET
            } else {
                OPERATION_BUDGET
            };
        bounded(deadline, async {
            let bytes = Self::state_bytes(state)?;
            let _gate = self.mutations.lock().await;
            if cleanup && let Some(lock) = lock {
                let renewed = self.renew(lock, deadline).await?;
                self.write(&bytes, WriteLease::Renewed(&renewed), deadline)
                    .await
            } else {
                let lease = lock.map_or(WriteLease::Unlocked, WriteLease::Held);
                self.write(&bytes, lease, deadline).await
            }
        })
        .await
    }
    async fn release_with_budget(&self, id: &str, budget: Duration) -> BackendResult<()> {
        let deadline = Instant::now() + budget;
        bounded(deadline, async {
            let _gate = self.mutations.lock().await;
            self.release(id, deadline).await
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
        let deadline = Instant::now() + OPERATION_BUDGET;
        bounded(deadline, async {
            let _gate = self.mutations.lock().await;
            let Some(file) = self.observe(self.settings.path(), deadline).await? else {
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
        let deadline = Instant::now() + OPERATION_BUDGET;
        bounded(deadline, async {
            Ok(self
                .observe(self.settings.path(), deadline)
                .await?
                .map(|file| RawState::remote(file.bytes, self.location())))
        })
        .await
    }
    async fn write_state(&self, state: &StateFile) -> BackendResult<()> {
        self.write_with_budget(state, None, false).await
    }
    async fn acquire_lock(&self, operation: &str) -> BackendResult<LockInfo> {
        let deadline = Instant::now() + OPERATION_BUDGET;
        bounded(deadline, async {
            let _gate = self.mutations.lock().await;
            let expected = match self.lease(deadline).await? {
                Some((current, _)) if !current.is_expired() => {
                    return Err(BackendError::locked(&current));
                }
                Some((_, sha)) => Some(sha),
                None => None,
            };
            let lock = LockInfo::new(operation);
            let bytes = serde_json::to_vec(&lock).map_err(|_| GitHubError::CorruptLease)?;
            self.conditional(
                &self.settings.path().lock_path(),
                Some(&bytes),
                expected.as_ref(),
                Purpose::Acquire(&lock),
                deadline,
            )
            .await?;
            Ok(lock)
        })
        .await
    }
    async fn release_lock(&self, lock: &LockInfo) -> BackendResult<()> {
        self.release_with_budget(lock.id(), OPERATION_BUDGET).await
    }
    async fn renew_lock(&self, lock: &LockInfo) -> BackendResult<LockInfo> {
        let deadline = Instant::now() + OPERATION_BUDGET;
        bounded(deadline, async {
            let _gate = self.mutations.lock().await;
            self.renew(lock, deadline).await
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
        self.release_with_budget(lock.id(), CLEANUP_BUDGET).await
    }
    async fn force_unlock(&self, id: &str) -> BackendResult<()> {
        self.release_with_budget(id, OPERATION_BUDGET).await
    }
    async fn storage_readiness(&self) -> BackendResult<StorageReadiness> {
        let deadline = Instant::now() + OPERATION_BUDGET;
        bounded(deadline, async {
            match self.destination(deadline).await {
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
