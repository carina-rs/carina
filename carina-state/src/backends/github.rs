//! GitHub.com Contents storage and SHA-conditioned leases.
use async_trait::async_trait;
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    borrow::Cow,
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
/// Reviewer measurement on real GitHub (2026-11-15, #4436): comparing six
/// commits adding 0.9–20 MB files returned 32,493 bytes, with large patches
/// omitted. Small text diffs do include patches. Allow >32x headroom while
/// bounding that unrelated payload independently of the decoded state cap.
pub const MAX_COMPARE_BYTES: usize = 1024 * 1024;
pub const RAW_MEDIA_TYPE: &str = "application/vnd.github.raw+json";
pub const JSON_MEDIA_TYPE: &str = "application/vnd.github+json";

/// Terminal protocol failures. Only sanitized API error excerpts enter
/// diagnostics; state/lease content and credentials are never included.
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
    #[error(
        "State write conflict: the state file had been deleted concurrently; the PUT recreated it and the created file has been kept"
    )]
    StateDeletedConcurrently,
    #[error("GitHub rate limit (HTTP {status}); retry hint: {retry_after:?}")]
    RateLimited {
        status: u16,
        retry_after: Option<Duration>,
        request_id: Option<String>,
    },
    #[error("GitHub state size {size} decoded bytes exceeds the cap of {cap} decoded bytes (base64 content: {encoded} bytes, plus request metadata)", encoded = .size.div_ceil(3).saturating_mul(4))]
    TooLarge { size: usize, cap: usize },
    #[error(
        "GitHub write was committed but could not be verified: compare response size {size} bytes exceeds its {cap}-byte cap"
    )]
    CompareTooLarge { size: usize, cap: usize },
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
impl GitHubRequest {
    fn body_limit(&self) -> BodyLimit {
        // Routes are /repos/{owner}/{repo}/{endpoint}/... . A Contents path
        // may itself contain a directory named "compare".
        if self.method == Method::Get && self.path.split('/').nth(4) == Some("compare") {
            BodyLimit::Compare
        } else {
            BodyLimit::Contents
        }
    }
}

#[derive(Clone, Copy)]
enum BodyLimit {
    Contents,
    Compare,
}
impl BodyLimit {
    fn cap(self) -> usize {
        match self {
            Self::Contents => MAX_BYTES,
            Self::Compare => MAX_COMPARE_BYTES,
        }
    }
    fn check(self, size: usize) -> Result<(), GitHubError> {
        if size <= self.cap() {
            return Ok(());
        }
        Err(match self {
            Self::Contents => GitHubError::TooLarge {
                size,
                cap: MAX_BYTES,
            },
            Self::Compare => GitHubError::CompareTooLarge {
                size,
                cap: MAX_COMPARE_BYTES,
            },
        })
    }
}

pub struct GitHubResponse {
    pub status: u16,
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
}

/// Injectable, bounded transport. Implementations must cap streamed response
/// bodies at the request's Contents or compare limit, sanitize reflected credentials in error responses,
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
    // None = unknown. Raw export reads
    // deliberately never update this writer's precondition.
    expected_state: Mutex<Option<Expected>>,
    pending_lease: Mutex<Option<PendingLease>>,
    migration_logged: OnceLock<crate::MigrationInfo>,
}

const OPERATION_BUDGET: Duration = Duration::from_secs(30);
// The existing finalizer allows 1s to flush and 2s overall. Each complete
// cleanup operation, including its gate, retries and compare, is bounded to 900ms.
const CLEANUP_BUDGET: Duration = Duration::from_millis(900);
const MAX_ATTEMPTS: usize = 5;
// A locked save needs strictly more than 60s remaining, plus 5s for clock
// skew. This narrows expiry/takeover races without renewing during cleanup.
const WRITE_TTL_MARGIN: Duration = Duration::from_secs(60);
const CLOCK_SKEW_ALLOWANCE: Duration = Duration::from_secs(5);

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
#[derive(Clone)]
enum Expected {
    Known(Option<BlobSha>),
    Ambiguous(AmbiguousWrite),
}
#[derive(Clone)]
struct AmbiguousWrite {
    prior: Option<BlobSha>,
    sent: BlobSha,
}
#[derive(Clone)]
enum PendingLease {
    Write { id: String, write: AmbiguousWrite },
    Delete { id: String },
    Recreated { id: String },
}
enum Resolution {
    Ours,
    Unchanged,
    Conflict,
}
impl AmbiguousWrite {
    fn resolve(&self, current: Option<&BlobSha>) -> Resolution {
        if matching_write_is_ours(current, &self.sent) {
            Resolution::Ours
        } else if current == self.prior.as_ref() {
            Resolution::Unchanged
        } else {
            Resolution::Conflict
        }
    }
}

/// Decision (b) remains open with the orchestrator. Preserve the current
/// exact-bytes recovery policy, including possible resurrection, ONLY here.
/// A Git blob hash is an identity of bytes, not a fencing generation.
fn matching_write_is_ours(current: Option<&BlobSha>, sent: &BlobSha) -> bool {
    current == Some(sent)
}

enum Mutation<'a> {
    CreateState {
        bytes: &'a [u8],
        lease: WriteLease<'a>,
    },
    ReplaceState {
        bytes: &'a [u8],
        sha: &'a BlobSha,
        lease: WriteLease<'a>,
    },
    CreateLease {
        lock: &'a LockInfo,
    },
    TakeOverLease {
        lock: &'a LockInfo,
        sha: &'a BlobSha,
    },
    RenewLease {
        id: &'a str,
        bytes: &'a [u8],
        sha: &'a BlobSha,
    },
    DeleteLease {
        id: &'a str,
        sha: &'a BlobSha,
    },
}
impl Mutation<'_> {
    fn bytes(&self) -> BackendResult<Option<Cow<'_, [u8]>>> {
        match self {
            Self::CreateState { bytes, .. }
            | Self::ReplaceState { bytes, .. }
            | Self::RenewLease { bytes, .. } => Ok(Some(Cow::Borrowed(bytes))),
            Self::CreateLease { lock } | Self::TakeOverLease { lock, .. } => Ok(Some(Cow::Owned(
                serde_json::to_vec(lock).map_err(|_| GitHubError::CorruptLease)?,
            ))),
            Self::DeleteLease { .. } => Ok(None),
        }
    }
    fn expected(&self) -> Option<&BlobSha> {
        match self {
            Self::CreateState { .. } | Self::CreateLease { .. } => None,
            Self::ReplaceState { sha, .. }
            | Self::TakeOverLease { sha, .. }
            | Self::RenewLease { sha, .. }
            | Self::DeleteLease { sha, .. } => Some(sha),
        }
    }
    fn lease_to_verify(&self) -> Option<&LockInfo> {
        match self {
            Self::CreateState {
                lease: WriteLease::Held(lock),
                ..
            }
            | Self::ReplaceState {
                lease: WriteLease::Held(lock),
                ..
            } => Some(lock),
            Self::CreateState {
                lease: WriteLease::Unlocked,
                ..
            }
            | Self::ReplaceState {
                lease: WriteLease::Unlocked,
                ..
            }
            | Self::CreateLease { .. }
            | Self::TakeOverLease { .. }
            | Self::RenewLease { .. }
            | Self::DeleteLease { .. } => None,
        }
    }
    fn lost_race(
        &self,
        current: Option<&ObservedFile>,
        attempt: usize,
    ) -> BackendResult<BackendError> {
        Ok(match self {
            Self::CreateState { .. } | Self::ReplaceState { .. } => {
                BackendError::StateWriteConflict { attempts: attempt }
            }
            Self::CreateLease { lock } | Self::TakeOverLease { lock, .. } => match current {
                Some(file) => BackendError::locked(&decode_lease(&file.bytes)?),
                None => BackendError::LockNotHeld(lock.id().into()),
            },
            Self::RenewLease { id, .. } | Self::DeleteLease { id, .. } => {
                if let Some(file) = current {
                    decode_lease(&file.bytes)?;
                }
                BackendError::LockNotHeld((*id).into())
            }
        })
    }
    fn reconciled(&self, current: Option<&ObservedFile>, sent: &BlobSha) -> BackendResult<bool> {
        match self {
            Self::CreateState { .. }
            | Self::ReplaceState { .. }
            | Self::CreateLease { .. }
            | Self::TakeOverLease { .. }
            | Self::RenewLease { .. } => {
                Ok(matching_write_is_ours(current.map(|file| &file.sha), sent))
            }
            Self::DeleteLease { id, .. } => Ok(match current {
                None => true,
                Some(file) => decode_lease(&file.bytes)?.id() != *id,
            }),
        }
    }
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
            pending_lease: Mutex::new(None),
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
        let limit = request.body_limit();
        loop {
            let response = bounded(operation.deadline, async {
                self.transport
                    .execute(request.clone(), operation.deadline)
                    .await
                    .map_err(BackendError::from)
            })
            .await?;
            limit.check(response.body.len())?;
            if matches!(response.status, 403 | 429) {
                let primary = response
                    .headers
                    .get("x-ratelimit-remaining")
                    .is_some_and(|value| value == "0");
                let message = serde_json::from_slice::<Value>(&response.body)
                    .ok()
                    .and_then(|value| {
                        value
                            .get("message")
                            .and_then(Value::as_str)
                            .map(str::to_lowercase)
                    })
                    .unwrap_or_default();
                let secondary = message.contains("secondary rate limit");
                let rate_message = message.contains("rate limit") || message.contains("rate-limit");
                if response.status == 429
                    || (primary && rate_message)
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
            body: redact_response_body(&response.body)
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
        if self.lease_is_revoked(lock.id()) {
            return Err(BackendError::LockNotHeld(lock.id().into()));
        }
        match self.lease(operation).await? {
            Some((current, _))
                if current.id() == lock.id()
                    && current.time_remaining().to_std().is_ok_and(|remaining| {
                        remaining > WRITE_TTL_MARGIN + CLOCK_SKEW_ALLOWANCE
                    }) =>
            {
                Ok(())
            }
            _ => Err(BackendError::LockNotHeld(lock.id().into())),
        }
    }
    fn remember_mutation(&self, mutation: &Mutation<'_>, sha: Option<BlobSha>) -> Option<BlobSha> {
        match mutation {
            Mutation::CreateState { .. } | Mutation::ReplaceState { .. } => {
                *self.expected_state.lock().expect("state revision mutex") =
                    Some(Expected::Known(sha.clone()));
            }
            Mutation::CreateLease { .. }
            | Mutation::TakeOverLease { .. }
            | Mutation::RenewLease { .. }
            | Mutation::DeleteLease { .. } => {}
        }
        sha
    }
    fn begin_mutation(&self, mutation: &Mutation<'_>, sent: &BlobSha) {
        let write = AmbiguousWrite {
            prior: mutation.expected().cloned(),
            sent: sent.clone(),
        };
        // Persist BEFORE awaiting dispatch: the outer timeout (or cancellation)
        // can drop this future before its error handler ever runs.
        match mutation {
            Mutation::CreateState { .. } | Mutation::ReplaceState { .. } => {
                *self.expected_state.lock().expect("state revision mutex") =
                    Some(Expected::Ambiguous(write));
            }
            Mutation::CreateLease { lock } | Mutation::TakeOverLease { lock, .. } => {
                *self.pending_lease.lock().expect("lease revision mutex") =
                    Some(PendingLease::Write {
                        id: lock.id().into(),
                        write,
                    });
            }
            Mutation::RenewLease { id, .. } => {
                *self.pending_lease.lock().expect("lease revision mutex") =
                    Some(PendingLease::Write {
                        id: (*id).into(),
                        write,
                    });
            }
            Mutation::DeleteLease { id, .. } => {
                *self.pending_lease.lock().expect("lease revision mutex") =
                    Some(PendingLease::Delete { id: (*id).into() });
            }
        }
    }
    fn finish_mutation(&self, mutation: &Mutation<'_>) {
        match mutation {
            Mutation::CreateState { .. } | Mutation::ReplaceState { .. } => {}
            Mutation::CreateLease { .. }
            | Mutation::TakeOverLease { .. }
            | Mutation::RenewLease { .. }
            | Mutation::DeleteLease { .. } => {
                *self.pending_lease.lock().expect("lease revision mutex") = None;
            }
        }
    }
    fn reject_mutation(&self, mutation: &Mutation<'_>) {
        self.remember_mutation(mutation, mutation.expected().cloned());
        self.finish_mutation(mutation);
    }
    fn forget_mutation(&self, mutation: &Mutation<'_>) {
        match mutation {
            Mutation::CreateState { .. } | Mutation::ReplaceState { .. } => {
                *self.expected_state.lock().expect("state revision mutex") = None;
            }
            Mutation::CreateLease { .. }
            | Mutation::TakeOverLease { .. }
            | Mutation::RenewLease { .. }
            | Mutation::DeleteLease { .. } => {
                *self.pending_lease.lock().expect("lease revision mutex") = None;
            }
        }
    }
    fn resolve_pending_lease(&self, current: Option<&BlobSha>) -> Option<(Resolution, String)> {
        let mut pending = self.pending_lease.lock().expect("lease revision mutex");
        match pending.as_ref()? {
            PendingLease::Write { id, write } => {
                let resolution = write.resolve(current);
                let id = id.clone();
                if matches!(resolution, Resolution::Ours | Resolution::Unchanged) {
                    *pending = None;
                }
                Some((resolution, id))
            }
            PendingLease::Delete { .. } => None,
            PendingLease::Recreated { id } => {
                let id = id.clone();
                if current.is_none() {
                    *pending = None;
                    Some((Resolution::Unchanged, id))
                } else {
                    Some((Resolution::Conflict, id))
                }
            }
        }
    }
    fn lease_is_revoked(&self, id: &str) -> bool {
        match self
            .pending_lease
            .lock()
            .expect("lease revision mutex")
            .as_ref()
        {
            Some(
                PendingLease::Delete { id: pending_id }
                | PendingLease::Recreated { id: pending_id },
            ) => pending_id == id,
            Some(PendingLease::Write { .. }) | None => false,
        }
    }
    async fn verify_reachable(
        &self,
        commit: &CommitSha,
        operation: &Operation,
    ) -> BackendResult<()> {
        // Real GitHub reviewer measurement, 2026-11-15 (#4436): both
        // compare/{sha}...exp%2Fslash-branch and ...exp/slash-branch returned
        // "ahead". Component encoding is valid for compare's head branch too.
        let response = self
            .get(
                format!(
                    "{}/compare/{}...{}",
                    self.repository_route(),
                    commit.as_str(),
                    encode_component(self.settings.branch().as_str())
                ),
                None,
                JSON_MEDIA_TYPE,
                operation,
            )
            .await?;
        if response.status != 200 {
            return Err(Self::api_error(&response));
        }
        #[derive(Deserialize)]
        struct Comparison {
            status: String,
            ahead_by: u64,
            behind_by: u64,
        }
        // Ignore commits, files and patches without constructing a Value tree.
        let comparison: Comparison =
            serde_json::from_slice(&response.body).map_err(|_| GitHubError::MalformedResponse)?;
        // A pinned Contents read only proves a detached commit's contents.
        // Compare checks that the acknowledged commit is on the branch, even
        // when later writers have advanced it. It does not fence later writes.
        let reachable = comparison.behind_by == 0
            && match comparison.status.as_str() {
                "identical" => comparison.ahead_by == 0,
                "ahead" => comparison.ahead_by > 0,
                _ => false,
            };
        if !reachable {
            return Err(GitHubError::UnexpectedOutcome {
                reason: "acknowledged commit is not on the branch; possible competing winners",
            }
            .into());
        }
        Ok(())
    }
    async fn conditional(
        &self,
        mutation: Mutation<'_>,
        operation: &Operation,
    ) -> BackendResult<Option<BlobSha>> {
        let lease_path = self.settings.path().lock_path();
        let (path, method, message) = match &mutation {
            Mutation::CreateState { .. } | Mutation::ReplaceState { .. } => (
                ContentsPath::State(self.settings.path()),
                Method::Put,
                "carina: update state",
            ),
            Mutation::CreateLease { .. }
            | Mutation::TakeOverLease { .. }
            | Mutation::RenewLease { .. } => (
                ContentsPath::Lease(&lease_path),
                Method::Put,
                "carina: update lease",
            ),
            Mutation::DeleteLease { .. } => (
                ContentsPath::Lease(&lease_path),
                Method::Delete,
                "carina: release lease",
            ),
        };
        let bytes = mutation.bytes()?;
        if let Some(bytes) = bytes.as_deref() {
            check_size(bytes.len())?;
        }
        let sent = blob_sha(bytes.as_deref().unwrap_or_default());
        let expected = mutation.expected();
        let mut body = json!({"message":message,"branch":self.settings.branch().as_str()});
        if let Some(sha) = expected {
            body["sha"] = json!(sha.as_str());
        }
        if let Some(bytes) = bytes.as_deref() {
            body["content"] = json!(STANDARD.encode(bytes));
        }
        let request = GitHubRequest {
            method,
            path: self.contents_route(path),
            reference: None,
            accept: JSON_MEDIA_TYPE,
            body: Some(body),
        };
        for attempt in 1..=MAX_ATTEMPTS {
            if let Some(lock) = mutation.lease_to_verify() {
                self.verify_owner(lock, operation).await?;
            }
            self.begin_mutation(&mutation, &sent);
            let rate_retries_before = operation.rate_retries.load(Ordering::Relaxed);
            let response = match self.request(request.clone(), operation).await {
                Ok(response) => response,
                Err(
                    error @ BackendError::GitHub(
                        GitHubError::TransportDeadline { .. } | GitHubError::Deadline,
                    ),
                ) => {
                    // There is no time left to observe. The next mutation uses
                    // a fresh operation budget to resolve the persisted intent.
                    return Err(error);
                }
                Err(
                    error @ BackendError::GitHub(
                        GitHubError::Transport { .. } | GitHubError::RateLimited { .. },
                    ),
                ) => {
                    if Instant::now() < operation.deadline
                        && let Ok(current) = self.observe(path, operation).await
                        && mutation.reconciled(current.as_ref(), &sent)?
                    {
                        let sha = self.remember_mutation(&mutation, current.map(|file| file.sha));
                        self.finish_mutation(&mutation);
                        return Ok(sha);
                    }
                    return Err(error);
                }
                Err(error) => {
                    self.reject_mutation(&mutation);
                    return Err(error);
                }
            };
            // A committed DELETE can lose its reply to a rate limit, then
            // receive 404/409 on the automatic retry. Its next observation is
            // authoritative for release, without deleting a successor lease.
            if operation.rate_retries.load(Ordering::Relaxed) > rate_retries_before
                && matches!(&mutation, Mutation::DeleteLease { .. })
                && matches!(response.status, 404 | 409 | 422)
            {
                let current = self.observe(path, operation).await?;
                if mutation.reconciled(current.as_ref(), &sent)? {
                    self.finish_mutation(&mutation);
                    return Ok(None);
                }
            }
            if response.status == 409 || (response.status == 422 && method == Method::Put) {
                let current = self.observe(path, operation).await?;
                // A definitive conflict is not a lost DELETE reply: preserve
                // the typed loser if the lease was concurrently replaced.
                let reconciled = match &mutation {
                    Mutation::CreateState { .. }
                    | Mutation::ReplaceState { .. }
                    | Mutation::CreateLease { .. }
                    | Mutation::TakeOverLease { .. }
                    | Mutation::RenewLease { .. } => {
                        mutation.reconciled(current.as_ref(), &sent)?
                    }
                    Mutation::DeleteLease { .. } => false,
                };
                if reconciled {
                    let sha = self.remember_mutation(&mutation, current.map(|file| file.sha));
                    self.finish_mutation(&mutation);
                    return Ok(sha);
                }
                if response.status == 422 {
                    self.reject_mutation(&mutation);
                    if expected.is_none() && current.is_some() {
                        return Err(mutation.lost_race(current.as_ref(), attempt)?);
                    }
                    return Err(Self::api_error(&response));
                }
                let vanished_takeover = match &mutation {
                    Mutation::TakeOverLease { .. } => current.is_none(),
                    Mutation::CreateState { .. }
                    | Mutation::ReplaceState { .. }
                    | Mutation::CreateLease { .. }
                    | Mutation::RenewLease { .. }
                    | Mutation::DeleteLease { .. } => false,
                };
                if current.as_ref().map(|file| &file.sha) != expected && !vanished_takeover {
                    self.reject_mutation(&mutation);
                    return Err(mutation.lost_race(current.as_ref(), attempt)?);
                }
                self.reject_mutation(&mutation);
                if attempt == MAX_ATTEMPTS {
                    return Err(GitHubError::Contention { attempts: attempt }.into());
                }
                let jitter = (uuid::Uuid::new_v4().as_u128() % 25) as u64;
                let delay = Duration::from_millis(10 * attempt as u64 + jitter);
                if delay >= operation.deadline.saturating_duration_since(Instant::now()) {
                    return Err(GitHubError::Deadline.into());
                }
                tokio::time::sleep(delay).await;
                continue;
            }
            if !matches!(response.status, 200 | 201) {
                self.reject_mutation(&mutation);
                return Err(Self::api_error(&response));
            }
            let malformed = || GitHubError::UnexpectedOutcome {
                reason: "successful mutation returned malformed metadata",
            };
            let value: Value = serde_json::from_slice(&response.body).map_err(|_| malformed())?;
            let success = match &mutation {
                Mutation::CreateState { .. }
                | Mutation::ReplaceState { .. }
                | Mutation::CreateLease { .. }
                | Mutation::TakeOverLease { .. }
                | Mutation::RenewLease { .. } => {
                    let sha = BlobSha::parse(
                        json_string(&value["content"], "sha").map_err(|_| malformed())?,
                    )
                    .map_err(|_| malformed())?;
                    if sha != sent {
                        // Never cache an acknowledgement for unrelated bytes.
                        self.forget_mutation(&mutation);
                        return Err(GitHubError::UnexpectedOutcome {
                            reason: "acknowledged blob SHA differs from the requested bytes",
                        }
                        .into());
                    }
                    Some(sha)
                }
                Mutation::DeleteLease { .. } => {
                    if value.get("content") != Some(&Value::Null) || response.status != 200 {
                        return Err(malformed().into());
                    }
                    None
                }
            };
            // Only a hash-verified acknowledgement updates the state cache.
            // Keep lease intent until the operation returns successfully, so a
            // compare timeout cannot strand an acquisition's UUID either.
            let success = self.remember_mutation(&mutation, success);
            if response.status == 201 {
                match &mutation {
                    Mutation::RenewLease { id, .. } => {
                        // The reply is known: this UUID has lost ownership.
                        // Cancellation during compare must not turn the known
                        // recreation into matching-bytes recovery on retry.
                        *self.pending_lease.lock().expect("lease revision mutex") =
                            Some(PendingLease::Recreated { id: (*id).into() });
                    }
                    Mutation::CreateState { .. }
                    | Mutation::ReplaceState { .. }
                    | Mutation::CreateLease { .. }
                    | Mutation::TakeOverLease { .. }
                    | Mutation::DeleteLease { .. } => {}
                }
            }
            let commit = (|| -> Result<_, GitHubError> {
                let commit = CommitSha::parse(json_string(&value["commit"], "sha")?)?;
                let parents = value["commit"]["parents"]
                    .as_array()
                    .filter(|parents| parents.len() == 1)
                    .ok_or(GitHubError::MalformedResponse)?;
                CommitSha::parse(json_string(&parents[0], "sha")?)?;
                Ok(commit)
            })()
            .map_err(|_| GitHubError::UnexpectedOutcome {
                reason: "successful mutation returned malformed commit metadata",
            })?;
            let verification = self.verify_reachable(&commit, operation).await;
            if response.status == 201 {
                let deletion_error = match &mutation {
                    Mutation::CreateState { .. }
                    | Mutation::CreateLease { .. }
                    | Mutation::TakeOverLease { .. } => None,
                    Mutation::ReplaceState { .. } => {
                        Some(GitHubError::StateDeletedConcurrently.into())
                    }
                    Mutation::RenewLease { id, .. } => {
                        // A known 201 must roll back only our returned SHA,
                        // even when compare failed. DELETE cannot recur here.
                        if Box::pin(
                            self.conditional(Mutation::DeleteLease { id, sha: &sent }, operation),
                        )
                        .await
                        .is_err()
                        {
                            log::warn!(
                                "GitHub recreated lease cleanup could not be confirmed; inspect the lock file (lock ID: {id})"
                            );
                        }
                        Some(BackendError::LockNotHeld((*id).into()))
                    }
                    Mutation::DeleteLease { .. } => Some(malformed().into()),
                };
                if let Some(error) = deletion_error {
                    return match verification {
                        Err(
                            error @ BackendError::GitHub(GitHubError::UnexpectedOutcome { .. }),
                        ) => Err(error),
                        _ => Err(error),
                    };
                }
            }
            verification?;
            self.finish_mutation(&mutation);
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
            Some(Expected::Known(sha)) => sha,
            unknown @ (None | Some(Expected::Ambiguous(_))) => {
                let commit = self.branch_snapshot(operation).await?;
                // Only the precondition is needed; never download a large
                // state's raw bytes just to learn its SHA during cleanup.
                let sha = self
                    .metadata_at(self.settings.path().into(), &commit, operation)
                    .await?
                    .map(|file| file.sha);
                if let Some(Expected::Ambiguous(write)) = unknown
                    && matches!(write.resolve(sha.as_ref()), Resolution::Conflict)
                {
                    return Err(BackendError::StateWriteConflict { attempts: 1 });
                }
                *self.expected_state.lock().expect("state revision mutex") =
                    Some(Expected::Known(sha.clone()));
                sha
            }
        };
        let mutation = match expected.as_ref() {
            Some(sha) => Mutation::ReplaceState { bytes, sha, lease },
            None => Mutation::CreateState { bytes, lease },
        };
        self.conditional(mutation, operation).await?;
        Ok(())
    }
    async fn renew(&self, lock: &LockInfo, operation: &Operation) -> BackendResult<LockInfo> {
        if self.lease_is_revoked(lock.id()) {
            // A dispatched DELETE may still finish after cancellation. Do not
            // renew a UUID while that release remains unresolved.
            return Err(BackendError::LockNotHeld(lock.id().into()));
        }
        let observed = self.lease(operation).await?;
        if matches!(
            self.resolve_pending_lease(observed.as_ref().map(|(_, sha)| sha)),
            Some((Resolution::Conflict, _))
        ) {
            return Err(BackendError::LockNotHeld(lock.id().into()));
        }
        let (current, sha) = observed.ok_or_else(|| BackendError::LockNotHeld(lock.id().into()))?;
        if current.id() != lock.id() || current.is_expired() {
            return Err(BackendError::LockNotHeld(lock.id().into()));
        }
        let renewed = lock.renewed();
        let bytes = serde_json::to_vec(&renewed).map_err(|_| GitHubError::CorruptLease)?;
        self.conditional(
            Mutation::RenewLease {
                id: lock.id(),
                bytes: &bytes,
                sha: &sha,
            },
            operation,
        )
        .await?;
        Ok(renewed)
    }
    async fn release(&self, id: &str, operation: &Operation) -> BackendResult<()> {
        let observed = self.lease(operation).await?;
        {
            let mut pending = self.pending_lease.lock().expect("lease revision mutex");
            if matches!(pending.as_ref(), Some(PendingLease::Delete { id: pending_id }) if pending_id == id)
                && observed.as_ref().is_none_or(|(lock, _)| lock.id() != id)
            {
                *pending = None;
                return Ok(());
            }
        }
        let (current, sha) = observed.ok_or_else(|| BackendError::LockNotFound(id.into()))?;
        if current.id() != id {
            return Err(BackendError::LockMismatch {
                expected: id.into(),
                actual: current.id().into(),
            });
        }
        self.conditional(Mutation::DeleteLease { id, sha: &sha }, operation)
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
            // A renewal adds a mutation and compare without fencing state
            // against lease takeover. Check ownership and the remaining-TTL
            // margin on every PUT attempt, within the same cleanup deadline.
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
fn blob_sha(bytes: &[u8]) -> BlobSha {
    use sha1::{Digest, Sha1};
    let mut hash = Sha1::new();
    hash.update(format!("blob {}\0", bytes.len()));
    hash.update(bytes);
    BlobSha::parse(&format!("{:x}", hash.finalize())).expect("locally computed git blob SHA")
}
fn decode_lease(bytes: &[u8]) -> Result<LockInfo, GitHubError> {
    let lock = LockInfo::from_json_slice(bytes).map_err(|_| GitHubError::CorruptLease)?;
    uuid::Uuid::parse_str(lock.id()).map_err(|_| GitHubError::CorruptLease)?;
    Ok(lock)
}
fn request_id(response: &GitHubResponse) -> Option<String> {
    response
        .headers
        .get("x-github-request-id")
        .map(|id| redact_url_queries(id))
}
// Response URLs can contain GitHub-issued raw tokens unrelated to our auth
// token (M4). Redact every URL query before storing an API excerpt in an
// error, so Display, Debug and downstream logs all receive the same safe text.
fn redact_response_body(body: &[u8]) -> String {
    fn redact_value(value: &mut Value) {
        match value {
            Value::String(text) => *text = redact_url_queries(text),
            Value::Array(values) => values.iter_mut().for_each(redact_value),
            Value::Object(values) => {
                let original = std::mem::take(values);
                for (key, mut value) in original {
                    redact_value(&mut value);
                    values.insert(redact_url_queries(&key), value);
                }
            }
            _ => {}
        }
    }
    if let Ok(mut value) = serde_json::from_slice::<Value>(body) {
        redact_value(&mut value);
        value.to_string()
    } else {
        redact_url_queries(&String::from_utf8_lossy(body).replace("\\/", "/"))
    }
}
fn redact_url_queries(mut text: &str) -> String {
    let mut safe = String::with_capacity(text.len());
    while let Some(scheme) = text.find("://") {
        let after_scheme = scheme + 3;
        let end = text[after_scheme..]
            .find(|ch: char| ch.is_whitespace() || matches!(ch, '\"' | '\'' | '<' | '>'))
            .map_or(text.len(), |end| after_scheme + end);
        let url = &text[..end];
        if let Some(query) = url[after_scheme..].find('?') {
            safe.push_str(&url[..after_scheme + query]);
            safe.push_str("?<redacted>");
        } else {
            safe.push_str(url);
        }
        text = &text[end..];
    }
    safe.push_str(text);
    safe
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
    if Instant::now() >= deadline {
        return Err(GitHubError::Deadline.into());
    }
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
                *self.expected_state.lock().expect("state revision mutex") =
                    Some(Expected::Known(None));
                return Ok(None);
            };
            let (state, migration) = crate::check_and_migrate_bytes(&file.bytes)
                .map_err(|error| error.with_state_read_remote_location(&self.location()))?
                .into_parts();
            *self.expected_state.lock().expect("state revision mutex") =
                Some(Expected::Known(Some(file.sha)));
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
            let observed = self.lease(&operation).await?;
            match self.resolve_pending_lease(observed.as_ref().map(|(_, sha)| sha)) {
                Some((Resolution::Ours, id)) => {
                    return observed
                        .map(|(lock, _)| lock)
                        .filter(|lock| !lock.is_expired())
                        .ok_or_else(|| BackendError::LockNotHeld(id));
                }
                Some((Resolution::Conflict, id)) => {
                    return Err(match observed {
                        Some((lock, _)) => BackendError::locked(&lock),
                        None => BackendError::LockNotHeld(id),
                    });
                }
                Some((Resolution::Unchanged, _)) | None => {}
            }
            let expected = match observed {
                Some((current, _)) if !current.is_expired() => {
                    return Err(BackendError::locked(&current));
                }
                Some((_, sha)) => Some(sha),
                None => None,
            };
            let lock = LockInfo::new(operation_name);
            let mutation = match expected.as_ref() {
                Some(sha) => Mutation::TakeOverLease { lock: &lock, sha },
                None => Mutation::CreateLease { lock: &lock },
            };
            self.conditional(mutation, &operation).await?;
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
