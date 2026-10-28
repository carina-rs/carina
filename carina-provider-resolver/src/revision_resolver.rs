//! Resolve provider binaries from GitHub Actions CI artifacts by git revision.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use carina_core::hint::ProjectCommand;
use carina_core::provider_source::GithubSource;
use sha2::{Digest, Sha256};

#[cfg(test)]
use crate::github::GithubWorkflowRunsResponse;
use crate::github::{
    GithubApi, GithubApiError, GithubArtifactResponse, GithubHttp, GithubWorkflowRunResponse,
    UreqGithubHttp, byte_size_label,
};
use crate::github_location::{ArtifactId, CommitSha, GitRef, RepoSlug, WorkflowRunId};
use crate::install_lock::{
    GlobalArtifactPath, GuardedLockFile, ProviderInstallLock, StagedArtifact,
    global_cache_artifact_path, install_global_artifact_atomically,
};
use crate::provider_resolver::{LockMode, WasmArtifactPath};
use crate::verified_artifact::{
    ArtifactPin, DownloadedArtifact, RevisionArtifactPin, VerifiedArtifact, VerifiedArtifactError,
    lookup_revision_artifact_pin,
};
#[cfg(test)]
use crate::verified_artifact::{
    publish_download, publish_unchanged_revision_download, revision_artifact_replacement_remedy,
};

/// GitHub Actions artifact metadata (subset used by the resolver).
struct Artifact {
    id: ArtifactId,
    name: String,
    expired: bool,
    size_in_bytes: Option<u64>,
}

#[derive(Debug)]
enum ArtifactDownloadError {
    MissingOrGone(String),
    Authentication(String),
    RateLimited(String),
    ResponseBodyTooLarge(String),
    Other(String),
}

impl ArtifactDownloadError {
    fn from_github(error: GithubApiError) -> Self {
        let message = error.to_string();
        match error {
            GithubApiError::Status {
                status: 404 | 410, ..
            } => Self::MissingOrGone(message),
            GithubApiError::Authentication(_)
            | GithubApiError::Status {
                status: 401 | 403, ..
            } => Self::Authentication(message),
            GithubApiError::RateLimited { .. } => Self::RateLimited(message),
            GithubApiError::ResponseBodyTooLarge { .. } => Self::ResponseBodyTooLarge(message),
            _ => Self::Other(message),
        }
    }

    fn is_missing_or_gone(&self) -> bool {
        matches!(self, Self::MissingOrGone(_))
    }

    fn is_retryable(&self) -> bool {
        matches!(self, Self::RateLimited(_) | Self::Other(_))
    }

    #[cfg(test)]
    fn missing_or_gone(status: u16) -> Self {
        Self::MissingOrGone(format!(
            "GitHub API returned status {status} when downloading artifact"
        ))
    }
}

impl fmt::Display for ArtifactDownloadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingOrGone(message)
            | Self::Authentication(message)
            | Self::RateLimited(message)
            | Self::ResponseBodyTooLarge(message)
            | Self::Other(message) => formatter.write_str(message),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum WorkflowRunRejectionReason {
    MissingEvent,
    EventIsNotPush,
    MissingHeadRepository,
    HeadRepositoryMismatch,
    MissingHeadSha,
    HeadShaMismatch,
}

impl fmt::Display for WorkflowRunRejectionReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::MissingEvent => "event is missing",
            Self::EventIsNotPush => "event is not push",
            Self::MissingHeadRepository => "head repository is missing",
            Self::HeadRepositoryMismatch => "head repository does not match the source repository",
            Self::MissingHeadSha => "head SHA is missing",
            Self::HeadShaMismatch => "head SHA does not match the requested SHA",
        })
    }
}

#[derive(Debug, PartialEq, Eq)]
struct WorkflowRunRejection {
    run_id: WorkflowRunId,
    event: Option<String>,
    head_repository: Option<String>,
    head_sha: Option<CommitSha>,
    reason: WorkflowRunRejectionReason,
}

impl fmt::Display for WorkflowRunRejection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let event = self.event.as_deref().unwrap_or("<missing>");
        let head_repository = self.head_repository.as_deref().unwrap_or("<missing>");
        let head_sha = self
            .head_sha
            .as_ref()
            .map(CommitSha::as_str)
            .unwrap_or("<missing>");
        write!(
            formatter,
            "run {} (event: {event}, head repository: {head_repository}, head SHA: {head_sha}) [{}]",
            self.run_id, self.reason
        )
    }
}

#[derive(Debug, PartialEq, Eq)]
struct WorkflowRunSelection {
    expected_repository: String,
    eligible_run_ids: Vec<WorkflowRunId>,
    rejected_runs: Vec<WorkflowRunRejection>,
}

impl WorkflowRunSelection {
    fn into_eligible_run_ids(self) -> Result<Vec<WorkflowRunId>, NoEligibleWorkflowRuns> {
        if self.eligible_run_ids.is_empty() {
            Err(NoEligibleWorkflowRuns {
                expected_repository: self.expected_repository,
                rejected_runs: self.rejected_runs,
            })
        } else {
            Ok(self.eligible_run_ids)
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct NoEligibleWorkflowRuns {
    expected_repository: String,
    rejected_runs: Vec<WorkflowRunRejection>,
}

impl fmt::Display for NoEligibleWorkflowRuns {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let rejected_runs = if self.rejected_runs.is_empty() {
            "(none)".to_string()
        } else {
            self.rejected_runs
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("; ")
        };
        write!(
            formatter,
            "Only push runs from {} are used. Rejected runs: {rejected_runs}.",
            self.expected_repository
        )
    }
}

fn filter_workflow_runs(
    source: &GithubSource,
    sha: &CommitSha,
    workflow_runs: Vec<GithubWorkflowRunResponse>,
) -> WorkflowRunSelection {
    let expected_repository = format!("{}/{}", source.owner(), source.repo());
    let mut eligible_run_ids = Vec::new();
    let mut rejected_runs = Vec::new();

    for run in workflow_runs {
        let head_repository = run
            .head_repository
            .and_then(|repository| repository.full_name);
        let head_sha = run.head_sha;
        let reason = match run.event.as_deref() {
            None => Some(WorkflowRunRejectionReason::MissingEvent),
            Some(event) if event != "push" => Some(WorkflowRunRejectionReason::EventIsNotPush),
            Some(_) => match head_repository.as_deref() {
                None => Some(WorkflowRunRejectionReason::MissingHeadRepository),
                Some(repository) if !repository.eq_ignore_ascii_case(&expected_repository) => {
                    Some(WorkflowRunRejectionReason::HeadRepositoryMismatch)
                }
                Some(_) => match head_sha.as_ref() {
                    None => Some(WorkflowRunRejectionReason::MissingHeadSha),
                    Some(run_sha) if run_sha != sha => {
                        Some(WorkflowRunRejectionReason::HeadShaMismatch)
                    }
                    Some(_) => None,
                },
            },
        };

        match reason {
            Some(reason) => rejected_runs.push(WorkflowRunRejection {
                run_id: run.id,
                event: run.event,
                head_repository,
                head_sha,
                reason,
            }),
            None => eligible_run_ids.push(run.id),
        }
    }

    WorkflowRunSelection {
        expected_repository,
        eligible_run_ids,
        rejected_runs,
    }
}

/// Obtain a GitHub token for API authentication.
///
/// Tries `GITHUB_TOKEN` env var first, then falls back to `gh auth token`.
pub fn get_github_token() -> Result<String, String> {
    if let Ok(token) = std::env::var("GITHUB_TOKEN")
        && !token.is_empty()
    {
        return Ok(token);
    }

    let output = std::process::Command::new("gh")
        .args(["auth", "token", "--hostname", "github.com"])
        .output()
        .map_err(|e| {
            format!(
                "Failed to run `gh auth token --hostname github.com`: {e}. Set GITHUB_TOKEN or install gh CLI."
            )
        })?;

    if !output.status.success() {
        return Err(
            "Failed to get GitHub token. Set GITHUB_TOKEN environment variable or run `gh auth login`.".to_string()
        );
    }

    let token = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if token.is_empty() {
        return Err("GitHub token is empty. Run `gh auth login` or set GITHUB_TOKEN.".to_string());
    }
    Ok(token)
}

/// Resolve a git ref (branch, tag, or SHA) to a full commit SHA.
pub fn resolve_ref_to_sha(
    source: &GithubSource,
    revision: &str,
    token: &str,
) -> Result<CommitSha, String> {
    let http = UreqGithubHttp;
    let api = GithubApi::new(&http, Some(token));
    resolve_ref_to_sha_with_api(source, revision, &api)
}

fn resolve_ref_to_sha_with_api<H: GithubHttp + ?Sized>(
    source: &GithubSource,
    revision: &str,
    api: &GithubApi<'_, H>,
) -> Result<CommitSha, String> {
    let (repo, git_ref) = validate_revision_location(source, revision)?;
    resolve_typed_ref_to_sha(source, revision, &repo, &git_ref, api)
}

fn validate_revision_location(
    source: &GithubSource,
    revision: &str,
) -> Result<(RepoSlug, GitRef), String> {
    let repo = RepoSlug::try_from(source).map_err(|error| error.to_string())?;
    let git_ref = GitRef::new(revision.to_string()).map_err(|error| error.to_string())?;
    Ok((repo, git_ref))
}

fn resolve_typed_ref_to_sha<H: GithubHttp + ?Sized>(
    source: &GithubSource,
    revision: &str,
    repo: &RepoSlug,
    git_ref: &GitRef,
    api: &GithubApi<'_, H>,
) -> Result<CommitSha, String> {
    api.commit(repo, git_ref)
        .map_err(|error| format!("Failed to resolve revision '{revision}' for {source}: {error}"))
}

/// Find eligible successful workflow runs for a given commit SHA.
/// Returns same-repository push run IDs newest-first, matching GitHub's default ordering.
///
/// Multiple workflows may run on the same commit (e.g. `ci.yml` and `docs.yml`
/// both triggered on push to main). The caller inspects artifacts from every
/// run to find the unique WASM entry — see
/// [`select_artifact_from_runs`].
fn find_workflow_run_ids<H: GithubHttp + ?Sized>(
    source: &GithubSource,
    repo: &RepoSlug,
    sha: &CommitSha,
    api: &GithubApi<'_, H>,
) -> Result<Vec<WorkflowRunId>, String> {
    let workflow_runs = api
        .workflow_runs(repo, sha)
        .map_err(|error| format!("Failed to fetch workflow runs for {source}@{sha}: {error}"))?;
    filter_workflow_runs(source, sha, workflow_runs)
        .into_eligible_run_ids()
        .map_err(|error| {
            format!("No eligible successful workflow runs found for {source}@{sha}. {error}")
        })
}

/// Fetch the artifacts list for a single workflow run.
fn fetch_run_artifacts<H: GithubHttp + ?Sized>(
    repo: &RepoSlug,
    run_id: WorkflowRunId,
    api: &GithubApi<'_, H>,
) -> Result<Vec<Artifact>, String> {
    api.run_artifacts(repo, run_id)
        .map(|artifacts| {
            artifacts
                .into_iter()
                .map(
                    |GithubArtifactResponse {
                         id,
                         name,
                         expired,
                         size_in_bytes,
                     }| Artifact {
                        id,
                        name,
                        expired,
                        size_in_bytes,
                    },
                )
                .collect()
        })
        .map_err(|error| format!("Failed to fetch artifacts for run {run_id}: {error}"))
}

#[derive(Debug)]
struct WasmZipEntry {
    name: String,
    bytes: Vec<u8>,
}

const MAX_WASM_ENTRY_DECOMPRESSED_SIZE: u64 = 256 * 1024 * 1024;
const MAX_ARTIFACT_ZIP_COMPRESSED_SIZE: u64 = 512 * 1024 * 1024;

fn wasm_entries(zip_data: Vec<u8>) -> Result<Vec<WasmZipEntry>, String> {
    wasm_entries_with_limit(zip_data, MAX_WASM_ENTRY_DECOMPRESSED_SIZE)
}

fn wasm_entries_with_limit(
    zip_data: Vec<u8>,
    decompressed_size_limit: u64,
) -> Result<Vec<WasmZipEntry>, String> {
    let cursor = io::Cursor::new(zip_data);
    let mut archive = zip::ZipArchive::new(cursor)
        .map_err(|error| format!("Failed to open artifact ZIP: {error}"))?;
    let mut entries = Vec::new();
    for index in 0..archive.len() {
        let file = archive
            .by_index(index)
            .map_err(|error| format!("Failed to read ZIP entry: {error}"))?;
        if file.is_dir() || !file.name().ends_with(".wasm") {
            continue;
        }
        let name = file.name().to_string();
        let limit_label = byte_size_label(decompressed_size_limit);
        if file.size() > decompressed_size_limit {
            return Err(format!(
                "WASM entry '{name}' declares a decompressed size of {} bytes, which exceeds the {limit_label} limit",
                file.size()
            ));
        }
        let mut bytes = Vec::new();
        file.take(decompressed_size_limit.saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(|error| format!("Failed to read WASM entry '{name}': {error}"))?;
        if bytes.len() as u64 > decompressed_size_limit {
            return Err(format!(
                "WASM entry '{name}' decompressed data exceeds the {limit_label} limit"
            ));
        }
        entries.push(WasmZipEntry { name, bytes });
    }
    Ok(entries)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LockedArtifactOperation {
    Inspect,
    Download,
}

impl LockedArtifactOperation {
    fn verb(self) -> &'static str {
        match self {
            Self::Inspect => "inspect",
            Self::Download => "download",
        }
    }
}

mod revision_artifact_selection {
    use super::*;

    #[derive(Clone, Copy)]
    pub(super) enum RevisionArtifactPinState {
        Pinned,
        Unpinned,
    }

    impl RevisionArtifactPinState {
        fn expired_artifact_remedy(self, project_dir: &Path) -> String {
            let init = ProjectCommand::new("init", project_dir);
            match self {
                Self::Pinned => format!(
                    "If the rebuilt artifact differs from the pinned sha256, remove this provider's \
                     entry from carina-providers.lock and run `{init}`."
                ),
                Self::Unpinned => format!("Then run `{init}` again."),
            }
        }
    }

    /// A GitHub artifact selected by inspecting its ZIP contents.
    ///
    /// Its fields and constructor stay private to this module, so an arbitrary
    /// artifact ID cannot acquire selected-artifact provenance.
    #[derive(Debug)]
    pub(super) struct SelectedRevisionArtifact {
        artifact_id: ArtifactId,
        wasm_bytes: Vec<u8>,
        recovery: Option<RevisionArtifactRecovery>,
    }

    #[derive(Debug)]
    pub(super) struct RevisionArtifactRecovery {
        artifact_id: ArtifactId,
        reason: String,
    }

    impl RevisionArtifactRecovery {
        pub(super) fn unavailable(artifact_id: ArtifactId) -> Self {
            Self {
                artifact_id,
                reason: "was unavailable".to_string(),
            }
        }

        pub(super) fn different_commit(
            artifact_id: ArtifactId,
            actual_sha: &CommitSha,
            pinned_sha: &CommitSha,
        ) -> Self {
            Self {
                artifact_id,
                reason: format!(
                    "belongs to commit {actual_sha}, not the pinned commit {pinned_sha}"
                ),
            }
        }

        pub(super) fn artifact_id(&self) -> ArtifactId {
            self.artifact_id
        }

        pub(super) fn reason(&self) -> &str {
            &self.reason
        }
    }

    impl SelectedRevisionArtifact {
        fn new(artifact_id: ArtifactId, wasm_bytes: Vec<u8>) -> Self {
            Self {
                artifact_id,
                wasm_bytes,
                recovery: None,
            }
        }

        fn recovered_from(mut self, recovery: RevisionArtifactRecovery) -> Self {
            self.recovery = Some(recovery);
            self
        }

        pub(super) fn artifact_id(&self) -> ArtifactId {
            self.artifact_id
        }

        pub(super) fn wasm_bytes(&self) -> &[u8] {
            &self.wasm_bytes
        }

        pub(super) fn recovery(&self) -> Option<&RevisionArtifactRecovery> {
            self.recovery.as_ref()
        }
    }

    struct CandidateWasm {
        artifact_id: ArtifactId,
        artifact_name: String,
        entry: WasmZipEntry,
    }

    mod distinct_wasm_content {
        use super::*;

        pub(super) trait WasmContent {
            fn content(&self) -> &[u8];
        }

        pub(super) struct UniqueWasm<T>(T);

        impl<T> UniqueWasm<T> {
            pub(super) fn into_inner(self) -> T {
                self.0
            }
        }

        pub(super) enum UniqueWasmError<T> {
            Empty,
            Multiple(Vec<T>),
        }

        pub(super) fn unique_wasm<T: WasmContent>(
            mut entries: Vec<T>,
        ) -> Result<UniqueWasm<T>, UniqueWasmError<T>> {
            let distinct_hash_count = entries
                .iter()
                .map(|entry| <[u8; 32]>::from(Sha256::digest(entry.content())))
                .collect::<HashSet<_>>()
                .len();
            match distinct_hash_count {
                0 => Err(UniqueWasmError::Empty),
                1 => Ok(UniqueWasm(entries.remove(0))),
                _ => Err(UniqueWasmError::Multiple(entries)),
            }
        }
    }

    use distinct_wasm_content::{UniqueWasmError, WasmContent, unique_wasm};

    impl WasmContent for WasmZipEntry {
        fn content(&self) -> &[u8] {
            &self.bytes
        }
    }

    impl WasmContent for CandidateWasm {
        fn content(&self) -> &[u8] {
            &self.entry.bytes
        }
    }

    #[derive(Debug)]
    pub(super) enum LockedReplayError {
        Github {
            operation: LockedArtifactOperation,
            error: ArtifactDownloadError,
        },
        DifferentCommit(CommitSha),
        InvalidArtifact(String),
    }

    pub(super) fn select_artifact_from_runs<F, D>(
        run_ids: &[WorkflowRunId],
        source: &GithubSource,
        sha: &CommitSha,
        project_dir: &Path,
        pin_state: RevisionArtifactPinState,
        mut fetch: F,
        mut download: D,
    ) -> Result<SelectedRevisionArtifact, String>
    where
        F: FnMut(WorkflowRunId) -> Result<Vec<Artifact>, String>,
        D: FnMut(ArtifactId) -> Result<Vec<u8>, ArtifactDownloadError>,
    {
        if run_ids.is_empty() {
            return Err(format!(
                "No successful workflow runs found for {source}@{sha}. Ensure CI has run successfully."
            ));
        }

        let mut candidate_names = Vec::new();
        let mut expired_names = Vec::new();
        let mut wasm_candidates = Vec::new();
        for &run_id in run_ids {
            for artifact in fetch(run_id)? {
                if artifact.expired {
                    expired_names.push(artifact.name);
                    continue;
                }
                if let Some(size_in_bytes) = artifact.size_in_bytes
                    && size_in_bytes > MAX_ARTIFACT_ZIP_COMPRESSED_SIZE
                {
                    let limit_label = byte_size_label(MAX_ARTIFACT_ZIP_COMPRESSED_SIZE);
                    return Err(format!(
                        "Candidate artifact '{}' (id {}) reports a compressed size of \
                         {size_in_bytes} bytes, which exceeds the {limit_label} limit.",
                        artifact.name, artifact.id
                    ));
                }
                candidate_names.push(artifact.name.clone());
                let entries = wasm_entries(download(artifact.id).map_err(|error| {
                    format!(
                        "Failed to download candidate artifact '{}' (id {}): {error}",
                        artifact.name, artifact.id
                    )
                })?)
                .map_err(|error| {
                    format!(
                        "Failed to inspect candidate artifact '{}' (id {}): {error}",
                        artifact.name, artifact.id
                    )
                })?;
                wasm_candidates.extend(entries.into_iter().map(|entry| CandidateWasm {
                    artifact_id: artifact.id,
                    artifact_name: artifact.name.clone(),
                    entry,
                }));
            }
        }

        match unique_wasm(wasm_candidates) {
            Ok(candidate) => {
                // Runs are supplied newest-first, so the first occurrence of
                // identical content carries the newest artifact ID.
                let candidate = candidate.into_inner();
                Ok(SelectedRevisionArtifact::new(
                    candidate.artifact_id,
                    candidate.entry.bytes,
                ))
            }
            Err(UniqueWasmError::Empty)
                if candidate_names.is_empty() && !expired_names.is_empty() =>
            {
                let remedy = pin_state.expired_artifact_remedy(project_dir);
                Err(format!(
                    "All artifacts in successful workflow runs for {source}@{sha} have expired: {}. \
                     Re-run CI for this commit or push a new commit (or point `revision` at one). \
                     {remedy}",
                    expired_names.join(", "),
                ))
            }
            Err(UniqueWasmError::Empty) => {
                let candidates = if candidate_names.is_empty() {
                    "(none)".to_string()
                } else {
                    candidate_names.join(", ")
                };
                let expired = if expired_names.is_empty() {
                    String::new()
                } else {
                    format!(
                        " Expired artifacts (not inspected because they expired): {}.",
                        expired_names.join(", ")
                    )
                };
                Err(format!(
                    "No .wasm entry found in any non-expired artifact for {source}@{sha}. \
                     Candidate artifacts: {candidates}.{expired}"
                ))
            }
            Err(UniqueWasmError::Multiple(wasm_candidates)) => {
                let causes = wasm_candidates
                    .iter()
                    .map(|candidate| {
                        format!("{}: {}", candidate.artifact_name, candidate.entry.name)
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                Err(format!(
                    "Multiple .wasm entries found across candidate artifacts for {source}@{sha}: \
                     {causes}. Exactly one distinct .wasm content hash is required."
                ))
            }
        }
    }

    pub(super) fn select_recovery_artifact_from_runs<F, D>(
        run_ids: &[WorkflowRunId],
        source: &GithubSource,
        sha: &CommitSha,
        project_dir: &Path,
        recovery: RevisionArtifactRecovery,
        fetch: F,
        download: D,
    ) -> Result<SelectedRevisionArtifact, String>
    where
        F: FnMut(WorkflowRunId) -> Result<Vec<Artifact>, String>,
        D: FnMut(ArtifactId) -> Result<Vec<u8>, ArtifactDownloadError>,
    {
        select_artifact_from_runs(
            run_ids,
            source,
            sha,
            project_dir,
            RevisionArtifactPinState::Pinned,
            fetch,
            download,
        )
        .map(|selected| selected.recovered_from(recovery))
    }

    pub(super) fn replay_locked_artifact<M, D>(
        source: &GithubSource,
        sha: &CommitSha,
        artifact_id: ArtifactId,
        mut artifact_head_sha: M,
        mut download: D,
    ) -> Result<SelectedRevisionArtifact, LockedReplayError>
    where
        M: FnMut(ArtifactId) -> Result<Option<CommitSha>, ArtifactDownloadError>,
        D: FnMut(ArtifactId) -> Result<Vec<u8>, ArtifactDownloadError>,
    {
        let actual_head_sha =
            artifact_head_sha(artifact_id).map_err(|error| LockedReplayError::Github {
                operation: LockedArtifactOperation::Inspect,
                error,
            })?;
        if let Some(actual_head_sha) = actual_head_sha
            && &actual_head_sha != sha
        {
            return Err(LockedReplayError::DifferentCommit(actual_head_sha));
        }
        let zip_data = download(artifact_id).map_err(|error| LockedReplayError::Github {
            operation: LockedArtifactOperation::Download,
            error,
        })?;
        let entries = wasm_entries(zip_data).map_err(|error| {
            LockedReplayError::InvalidArtifact(format!(
                "Failed to inspect locked artifact {artifact_id} for {source}@{sha}: {error}"
            ))
        })?;
        let entry = match unique_wasm(entries) {
            Ok(entry) => entry.into_inner(),
            Err(error) => {
                let (found, entries) = match error {
                    UniqueWasmError::Empty => ("none", Vec::new()),
                    UniqueWasmError::Multiple(entries) => ("multiple distinct contents", entries),
                };
                let names = entries
                    .iter()
                    .map(|entry| entry.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                let names = if names.is_empty() { "(none)" } else { &names };
                return Err(LockedReplayError::InvalidArtifact(format!(
                    "Locked artifact {artifact_id} for {source}@{sha} must contain exactly one \
                     distinct .wasm content, found {found}: {names}. The pinned artifact itself \
                     is invalid; publishing another CI run cannot change it."
                )));
            }
        };
        Ok(SelectedRevisionArtifact::new(artifact_id, entry.bytes))
    }
}

use revision_artifact_selection::{
    LockedReplayError, RevisionArtifactPinState, RevisionArtifactRecovery,
    SelectedRevisionArtifact, replay_locked_artifact, select_artifact_from_runs,
    select_recovery_artifact_from_runs,
};

fn locked_artifact_error(
    operation: LockedArtifactOperation,
    source: &GithubSource,
    sha: &CommitSha,
    artifact_id: ArtifactId,
    error: &ArtifactDownloadError,
) -> String {
    let verb = operation.verb();
    let mut message =
        format!("Failed to {verb} locked artifact {artifact_id} for {source}@{sha}: {error}.");
    if error.is_retryable() {
        message.push_str(" Retry after resolving the GitHub API or network failure.");
    }
    message
}

struct RevisionArtifactLocation<'a> {
    source: &'a GithubSource,
    sha: &'a CommitSha,
    project_dir: &'a Path,
}

impl<'a> RevisionArtifactLocation<'a> {
    fn new(source: &'a GithubSource, sha: &'a CommitSha, project_dir: &'a Path) -> Self {
        Self {
            source,
            sha,
            project_dir,
        }
    }
}

fn resolve_revision_artifact<F, A, M, D>(
    artifact_pin: &ArtifactPin<'_, RevisionArtifactPin>,
    location: RevisionArtifactLocation<'_>,
    mode: LockMode,
    mut find_runs: F,
    mut fetch: A,
    mut artifact_head_sha: M,
    mut download: D,
) -> Result<SelectedRevisionArtifact, String>
where
    F: FnMut() -> Result<Vec<WorkflowRunId>, String>,
    A: FnMut(WorkflowRunId) -> Result<Vec<Artifact>, String>,
    M: FnMut(ArtifactId) -> Result<Option<CommitSha>, ArtifactDownloadError>,
    D: FnMut(ArtifactId) -> Result<Vec<u8>, ArtifactDownloadError>,
{
    let RevisionArtifactLocation {
        source,
        sha,
        project_dir,
    } = location;
    let pin_state = if artifact_pin.pinned().is_some() {
        RevisionArtifactPinState::Pinned
    } else {
        RevisionArtifactPinState::Unpinned
    };
    let mut prefetched_artifacts = HashMap::new();
    let mut fallback_run_ids = None;
    let mut recovery = None;
    if mode != LockMode::Upgrade
        && let Some((_lock_entry, pin)) = artifact_pin.pinned()
    {
        match replay_locked_artifact(
            source,
            sha,
            pin.artifact_id,
            &mut artifact_head_sha,
            &mut download,
        ) {
            Ok(selected) => return Ok(selected),
            Err(LockedReplayError::Github {
                operation: _,
                error,
            }) if error.is_missing_or_gone() => {
                recovery = Some(RevisionArtifactRecovery::unavailable(pin.artifact_id));
            }
            Err(LockedReplayError::DifferentCommit(actual_sha)) => {
                recovery = Some(RevisionArtifactRecovery::different_commit(
                    pin.artifact_id,
                    &actual_sha,
                    sha,
                ));
            }
            Err(LockedReplayError::Github {
                operation: LockedArtifactOperation::Inspect,
                error,
            }) => {
                return Err(locked_artifact_error(
                    LockedArtifactOperation::Inspect,
                    source,
                    sha,
                    pin.artifact_id,
                    &error,
                ));
            }
            Err(LockedReplayError::Github {
                operation: LockedArtifactOperation::Download,
                error,
            }) => {
                let run_ids = find_runs().map_err(|listing_error| {
                    format!(
                        "{} Could not check whether the artifact expired: {listing_error}",
                        locked_artifact_error(
                            LockedArtifactOperation::Download,
                            source,
                            sha,
                            pin.artifact_id,
                            &error,
                        )
                    )
                })?;
                let mut marked_expired = false;
                for &run_id in &run_ids {
                    let artifacts = fetch(run_id).map_err(|listing_error| {
                        format!(
                            "{} Could not check whether the artifact expired: {listing_error}",
                            locked_artifact_error(
                                LockedArtifactOperation::Download,
                                source,
                                sha,
                                pin.artifact_id,
                                &error,
                            )
                        )
                    })?;
                    marked_expired |= artifacts
                        .iter()
                        .any(|artifact| artifact.id == pin.artifact_id && artifact.expired);
                    prefetched_artifacts.insert(run_id, artifacts);
                }
                if !marked_expired {
                    return Err(locked_artifact_error(
                        LockedArtifactOperation::Download,
                        source,
                        sha,
                        pin.artifact_id,
                        &error,
                    ));
                }
                recovery = Some(RevisionArtifactRecovery::unavailable(pin.artifact_id));
                fallback_run_ids = Some(run_ids);
            }
            Err(LockedReplayError::InvalidArtifact(message)) => {
                return Err(message);
            }
        }
    }
    let run_ids = match fallback_run_ids {
        Some(run_ids) => run_ids,
        None => find_runs()?,
    };
    let mut fetch_artifacts = |run_id| match prefetched_artifacts.remove(&run_id) {
        Some(artifacts) => Ok(artifacts),
        None => fetch(run_id),
    };
    match recovery {
        Some(recovery) => select_recovery_artifact_from_runs(
            &run_ids,
            source,
            sha,
            project_dir,
            recovery,
            &mut fetch_artifacts,
            &mut download,
        ),
        None => select_artifact_from_runs(
            &run_ids,
            source,
            sha,
            project_dir,
            pin_state,
            &mut fetch_artifacts,
            &mut download,
        ),
    }
}

fn artifact_head_sha<H: GithubHttp + ?Sized>(
    repo: &RepoSlug,
    artifact_id: ArtifactId,
    api: &GithubApi<'_, H>,
) -> Result<Option<CommitSha>, ArtifactDownloadError> {
    api.artifact(repo, artifact_id)
        .map(|artifact| artifact.workflow_run.and_then(|run| run.head_sha))
        .map_err(ArtifactDownloadError::from_github)
}

/// Download a GitHub Actions artifact ZIP.
fn download_artifact_zip<H: GithubHttp + ?Sized>(
    repo: &RepoSlug,
    artifact_id: ArtifactId,
    api: &GithubApi<'_, H>,
) -> Result<Vec<u8>, ArtifactDownloadError> {
    api.download_artifact_zip(repo, artifact_id, MAX_ARTIFACT_ZIP_COMPRESSED_SIZE)
        .map_err(ArtifactDownloadError::from_github)
}

fn stage_selected_artifact<'lock>(
    install_lock: &'lock ProviderInstallLock,
    destination: &WasmArtifactPath,
    selected: &SelectedRevisionArtifact,
) -> Result<StagedArtifact<'lock>, String> {
    let mut staged = StagedArtifact::for_project(install_lock, destination).map_err(|error| {
        format!(
            "Failed to stage provider artifact {}: {error}",
            destination.as_path().display()
        )
    })?;
    staged
        .write_all(selected.wasm_bytes())
        .map_err(|error| format!("Failed to write selected WASM file: {error}"))?;
    Ok(staged)
}

/// Cache path for revision-based providers.
/// Uses `rev-{sha_prefix}` as the version directory.
pub fn cache_path_revision(base_dir: &Path, source: &GithubSource, sha: &CommitSha) -> PathBuf {
    let sha_prefix = sha.short();
    base_dir
        .join(".carina")
        .join("providers")
        .join(source.to_string())
        .join(format!("rev-{sha_prefix}"))
        .join(format!("{}.wasm", source.repo()))
}

pub(crate) fn global_cache_path_revision(
    source: &GithubSource,
    sha: &CommitSha,
) -> Option<GlobalArtifactPath> {
    let sha_prefix = sha.short();
    global_cache_artifact_path(
        PathBuf::from(source.to_string())
            .join(format!("rev-{sha_prefix}"))
            .join(format!("{}.wasm", source.repo())),
    )
}

mod revision_artifact_cache {
    use super::*;

    #[derive(Debug)]
    pub(super) enum PublishError<E> {
        Metadata(io::Error),
        Artifact(E),
    }

    /// Unverified revision cache metadata read alongside a cached WASM path.
    pub(super) struct CachedRevisionArtifact {
        artifact_id: ArtifactId,
        path: PathBuf,
    }

    impl CachedRevisionArtifact {
        pub(super) fn artifact_id(&self) -> ArtifactId {
            self.artifact_id
        }

        pub(super) fn path(&self) -> &Path {
            &self.path
        }
    }

    fn sidecar_path(wasm_path: &Path) -> PathBuf {
        let mut sidecar = wasm_path.as_os_str().to_os_string();
        sidecar.push(".artifact-id");
        PathBuf::from(sidecar)
    }

    fn read_artifact_id(wasm_path: &Path) -> Option<ArtifactId> {
        let file = std::fs::File::open(sidecar_path(wasm_path)).ok()?;
        let mut value = String::new();
        file.take(64).read_to_string(&mut value).ok()?;
        serde_json::from_str(value.trim()).ok()
    }

    pub(super) fn read(
        wasm_path: &Path,
        fallback_artifact_id: ArtifactId,
    ) -> Option<CachedRevisionArtifact> {
        wasm_path.exists().then(|| CachedRevisionArtifact {
            artifact_id: read_artifact_id(wasm_path).unwrap_or(fallback_artifact_id),
            path: wasm_path.to_path_buf(),
        })
    }

    fn write(wasm_path: &Path, artifact_id: ArtifactId) -> io::Result<()> {
        let sidecar = sidecar_path(wasm_path);
        let parent = sidecar.parent().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("cache metadata path {} has no parent", sidecar.display()),
            )
        })?;
        std::fs::create_dir_all(parent)?;
        let mut staged = tempfile::NamedTempFile::new_in(parent)?;
        writeln!(staged, "{artifact_id}")?;
        staged.as_file().sync_all()?;
        staged.persist(&sidecar).map_err(|error| error.error)?;
        if let Ok(directory) = std::fs::File::open(parent) {
            let _ = directory.sync_all();
        }
        Ok(())
    }

    pub(super) fn publish<T, E>(
        wasm_path: &Path,
        artifact_id: ArtifactId,
        publish_artifact: impl FnOnce() -> Result<T, E>,
    ) -> Result<T, PublishError<E>> {
        write(wasm_path, artifact_id).map_err(PublishError::Metadata)?;
        publish_artifact().map_err(PublishError::Artifact)
    }

    pub(super) fn remove(wasm_path: &Path) -> io::Result<()> {
        for path in [sidecar_path(wasm_path), wasm_path.to_path_buf()] {
            match std::fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }
}

use revision_artifact_cache::CachedRevisionArtifact;

mod verified_revision_artifact {
    use super::*;

    pub(super) struct VerifiedRevisionArtifact {
        artifact_id: ArtifactId,
        path: WasmArtifactPath,
        source: String,
        name: String,
        sha256: String,
    }

    pub(super) enum PublishedRevisionArtifact {
        Pinned {
            artifact: VerifiedRevisionArtifact,
            pin: RevisionArtifactPin,
        },
        FirstUse(VerifiedRevisionArtifact),
    }

    impl VerifiedRevisionArtifact {
        fn new(
            artifact_id: ArtifactId,
            path: WasmArtifactPath,
            source: &str,
            name: &str,
            sha256: String,
        ) -> Self {
            Self {
                artifact_id,
                path,
                source: source.to_string(),
                name: name.to_string(),
                sha256,
            }
        }

        fn from_verified(artifact_id: ArtifactId, artifact: VerifiedArtifact, name: &str) -> Self {
            Self::new(
                artifact_id,
                artifact.path().clone(),
                artifact.source(),
                name,
                artifact.sha256().to_string(),
            )
        }

        pub(super) fn artifact_id(&self) -> ArtifactId {
            self.artifact_id
        }

        pub(super) fn path(&self) -> &WasmArtifactPath {
            &self.path
        }

        pub(super) fn source(&self) -> &str {
            &self.source
        }

        pub(super) fn name(&self) -> &str {
            &self.name
        }

        pub(super) fn sha256(&self) -> &str {
            &self.sha256
        }
    }

    pub(super) fn from_existing<R>(
        cached: &CachedRevisionArtifact,
        lock_entry: &crate::provider_resolver::LockEntry<R>,
        name: &str,
        project_dir: &Path,
    ) -> Result<VerifiedRevisionArtifact, VerifiedArtifactError> {
        let artifact = VerifiedArtifact::from_existing(lock_entry, name, project_dir)?;
        if artifact.path().as_path() != cached.path() {
            return Err(VerifiedArtifactError::Other(format!(
                "Cached revision artifact path {} does not match lock-selected path {}",
                cached.path().display(),
                artifact.path().as_path().display(),
            )));
        }
        Ok(VerifiedRevisionArtifact::from_verified(
            cached.artifact_id(),
            artifact,
            name,
        ))
    }

    pub(super) fn install_project_artifact_from_global_cache<R>(
        cached: &CachedRevisionArtifact,
        install_lock: &ProviderInstallLock,
        destination: &WasmArtifactPath,
        lock_entry: &crate::provider_resolver::LockEntry<R>,
        name: &str,
    ) -> Result<VerifiedRevisionArtifact, VerifiedArtifactError> {
        revision_artifact_cache::publish(destination.as_path(), cached.artifact_id(), || {
            crate::install_lock::install_project_artifact_from_global_cache(
                install_lock,
                cached.path(),
                destination,
                lock_entry,
                name,
            )
            .map(|artifact| {
                VerifiedRevisionArtifact::from_verified(cached.artifact_id(), artifact, name)
            })
        })
        .map_err(|error| match error {
            revision_artifact_cache::PublishError::Metadata(error) => VerifiedArtifactError::Other(
                revision_cache_metadata_error(destination.as_path(), error),
            ),
            revision_artifact_cache::PublishError::Artifact(error) => error,
        })
    }

    fn from_downloaded(
        selected: &SelectedRevisionArtifact,
        downloaded: DownloadedArtifact<RevisionArtifactPin>,
        source: &str,
        name: &str,
    ) -> PublishedRevisionArtifact {
        match downloaded {
            DownloadedArtifact::Verified { artifact, pin } => PublishedRevisionArtifact::Pinned {
                artifact: VerifiedRevisionArtifact::from_verified(
                    selected.artifact_id(),
                    artifact,
                    name,
                ),
                pin,
            },
            DownloadedArtifact::FirstUse { path, sha256 } => PublishedRevisionArtifact::FirstUse(
                VerifiedRevisionArtifact::new(selected.artifact_id(), path, source, name, sha256),
            ),
        }
    }

    pub(super) fn publish_download(
        selected: &SelectedRevisionArtifact,
        install_lock: &ProviderInstallLock,
        destination: &WasmArtifactPath,
        pin: ArtifactPin<'_, RevisionArtifactPin>,
        source: &str,
        name: &str,
    ) -> Result<PublishedRevisionArtifact, String> {
        revision_artifact_cache::publish(destination.as_path(), selected.artifact_id(), || {
            let staged = stage_selected_artifact(install_lock, destination, selected)?;
            crate::verified_artifact::publish_download(staged, pin)
                .map(|downloaded| from_downloaded(selected, downloaded, source, name))
        })
        .map_err(|error| match error {
            revision_artifact_cache::PublishError::Metadata(error) => {
                revision_cache_metadata_error(destination.as_path(), error)
            }
            revision_artifact_cache::PublishError::Artifact(error) => error,
        })
    }

    pub(super) fn publish_unchanged_revision_download(
        selected: &SelectedRevisionArtifact,
        install_lock: &ProviderInstallLock,
        destination: &WasmArtifactPath,
        pin: ArtifactPin<'_, RevisionArtifactPin>,
        source: &str,
        name: &str,
    ) -> Result<PublishedRevisionArtifact, String> {
        revision_artifact_cache::publish(destination.as_path(), selected.artifact_id(), || {
            let staged = stage_selected_artifact(install_lock, destination, selected)?;
            crate::verified_artifact::publish_unchanged_revision_download(staged, pin)
                .map(|downloaded| from_downloaded(selected, downloaded, source, name))
        })
        .map_err(|error| match error {
            revision_artifact_cache::PublishError::Metadata(error) => {
                revision_cache_metadata_error(destination.as_path(), error)
            }
            revision_artifact_cache::PublishError::Artifact(error) => error,
        })
    }
}

use verified_revision_artifact::{PublishedRevisionArtifact, VerifiedRevisionArtifact};

fn revision_lock_entry(
    artifact: &VerifiedRevisionArtifact,
    revision: &str,
    resolved_sha: &CommitSha,
) -> crate::provider_resolver::LockEntry {
    crate::provider_resolver::LockEntry {
        name: artifact.name().to_string(),
        source: artifact.source().to_string(),
        kind: crate::provider_resolver::LockEntryKind::Revision {
            revision: revision.to_string(),
            resolved_sha: resolved_sha.clone(),
            artifact_id: artifact.artifact_id(),
        },
        sha256: artifact.sha256().to_string(),
        registry: None,
    }
}

fn revision_cache_metadata_error(wasm_path: &Path, error: io::Error) -> String {
    format!(
        "Failed to write revision cache metadata {}: {error}",
        wasm_path.display()
    )
}

fn install_global_revision_artifact(
    source: &Path,
    destination: &GlobalArtifactPath,
    artifact_id: ArtifactId,
) -> Result<(), String> {
    revision_artifact_cache::publish(destination.as_path(), artifact_id, || {
        install_global_artifact_atomically(source, destination)
    })
    .map_err(|error| match error {
        revision_artifact_cache::PublishError::Metadata(error) => {
            revision_cache_metadata_error(destination.as_path(), error)
        }
        revision_artifact_cache::PublishError::Artifact(error) => {
            let cleanup = revision_artifact_cache::remove(destination.as_path());
            match cleanup {
                Ok(()) => format!(
                    "Failed to install global revision cache artifact {}: {error}. The incomplete cache entry was removed.",
                    destination.as_path().display()
                ),
                Err(cleanup_error) => format!(
                    "Failed to install global revision cache artifact {}: {error}. Failed to remove the incomplete cache entry: {cleanup_error}.",
                    destination.as_path().display()
                ),
            }
        }
    })
}

fn revision_artifact_recovery_notice(
    recovery: &RevisionArtifactRecovery,
    replacement_artifact_id: ArtifactId,
    mode: LockMode,
    project_dir: &Path,
) -> Option<String> {
    if recovery.artifact_id() == replacement_artifact_id {
        return None;
    }
    let lock_status = match mode {
        LockMode::Locked => format!(
            "Running `{}` updates the lock.",
            ProjectCommand::new("init", project_dir)
        ),
        LockMode::Normal | LockMode::Upgrade => "The lock was updated.".to_string(),
    };
    Some(format!(
        "Pinned artifact {} {}; the artifact with the same sha256 was re-selected \
         ({replacement_artifact_id}). {lock_status}",
        recovery.artifact_id(),
        recovery.reason(),
    ))
}

struct RevisionGithubOps<R, F, A, M, D> {
    resolve_ref: R,
    list_runs: F,
    list_artifacts: A,
    artifact_head_sha: M,
    download_artifact: D,
}

/// Resolve a provider by revision: download from CI artifacts if not cached.
pub fn resolve_provider_by_revision(
    lock_file: &mut GuardedLockFile<'_>,
    source: &GithubSource,
    revision: &str,
    name: &str,
    mode: LockMode,
) -> Result<(WasmArtifactPath, CommitSha), String> {
    let token = get_github_token()?;
    let http = UreqGithubHttp;
    let api = GithubApi::new(&http, Some(&token));
    resolve_provider_by_revision_with_api(lock_file, source, revision, name, mode, &api)
}

pub(crate) fn resolve_provider_by_revision_with_api<H: GithubHttp + ?Sized>(
    lock_file: &mut GuardedLockFile<'_>,
    source: &GithubSource,
    revision: &str,
    name: &str,
    mode: LockMode,
    api: &GithubApi<'_, H>,
) -> Result<(WasmArtifactPath, CommitSha), String> {
    let (repo, git_ref) = validate_revision_location(source, revision)?;
    resolve_provider_by_revision_with(
        lock_file,
        source,
        revision,
        name,
        mode,
        RevisionGithubOps {
            resolve_ref: || resolve_typed_ref_to_sha(source, revision, &repo, &git_ref, api),
            list_runs: |sha: CommitSha| find_workflow_run_ids(source, &repo, &sha, api),
            list_artifacts: |run_id| fetch_run_artifacts(&repo, run_id, api),
            artifact_head_sha: |artifact_id| artifact_head_sha(&repo, artifact_id, api),
            download_artifact: |artifact_id| download_artifact_zip(&repo, artifact_id, api),
        },
    )
}

fn resolve_provider_by_revision_with<R, F, A, M, D>(
    lock_file: &mut GuardedLockFile<'_>,
    source: &GithubSource,
    revision: &str,
    name: &str,
    mode: LockMode,
    github: RevisionGithubOps<R, F, A, M, D>,
) -> Result<(WasmArtifactPath, CommitSha), String>
where
    R: FnMut() -> Result<CommitSha, String>,
    F: FnMut(CommitSha) -> Result<Vec<WorkflowRunId>, String>,
    A: FnMut(WorkflowRunId) -> Result<Vec<Artifact>, String>,
    M: FnMut(ArtifactId) -> Result<Option<CommitSha>, ArtifactDownloadError>,
    D: FnMut(ArtifactId) -> Result<Vec<u8>, ArtifactDownloadError>,
{
    let RevisionGithubOps {
        mut resolve_ref,
        mut list_runs,
        mut list_artifacts,
        mut artifact_head_sha,
        mut download_artifact,
    } = github;
    let install_lock = lock_file.install_lock();
    let base_dir = install_lock.project_dir();
    let source_key = source.source_key();
    let legacy_pin = lock_file.legacy_revision_pin(&source_key, revision);

    // 1. Resolve revision to SHA
    let sha = if mode != LockMode::Upgrade
        && let Some(lock_entry) = lock_file.find_by_source_key(&source_key)
        && let super::provider_resolver::LockEntryKind::Revision {
            revision: locked_revision,
            resolved_sha,
            artifact_id: _,
        } = &lock_entry.kind
        && locked_revision == revision
    {
        // Reuse locked SHA if revision matches (skip on upgrade)
        resolved_sha.clone()
    } else if mode != LockMode::Upgrade
        && let Some((resolved_sha, _sha256)) = &legacy_pin
    {
        resolved_sha.clone()
    } else {
        eprintln!(
            "Resolving revision '{}' for provider '{}'...",
            revision, name
        );
        resolve_ref()?
    };

    let legacy_pin = legacy_pin.filter(|(resolved_sha, _sha256)| resolved_sha == &sha);
    let artifact_pin = lookup_revision_artifact_pin(lock_file, &source_key, revision, &sha, name);
    if mode == LockMode::Locked && artifact_pin.pinned().is_none() {
        return Err(format!(
            "Locked resolution for provider '{name}' has no matching revision entry"
        ));
    }

    // As in version mode, machine-local artifacts are never first-use trust
    // anchors. A project or global cache entry without this exact revision pin
    // is skipped in favor of a fresh origin download.
    let wasm_path = WasmArtifactPath::new(cache_path_revision(base_dir, source, &sha), name)?;
    if mode != LockMode::Upgrade
        && let Some((lock_entry, pin)) = artifact_pin.pinned()
        && let Some(cached) = revision_artifact_cache::read(wasm_path.as_path(), pin.artifact_id)
    {
        let pin = pin.clone();
        let verified =
            match verified_revision_artifact::from_existing(&cached, lock_entry, name, base_dir) {
                Ok(verified) => verified,
                Err(VerifiedArtifactError::ChecksumMismatch(mismatch)) => {
                    let removal = revision_artifact_cache::remove(wasm_path.as_path());
                    return Err((*mismatch).after_cache_removal(wasm_path.as_path(), removal));
                }
                Err(VerifiedArtifactError::Other(message)) => return Err(message),
            };
        if mode != LockMode::Locked {
            lock_file.upsert(revision_lock_entry(
                &verified,
                &pin.revision,
                &pin.resolved_sha,
            ));
        }
        return Ok((verified.path().clone(), sha));
    }

    if mode != LockMode::Upgrade
        && let Some((lock_entry, pin)) = artifact_pin.pinned()
        && let Some(global_wasm) = global_cache_path_revision(source, &sha)
        && let Some(cached) = revision_artifact_cache::read(global_wasm.as_path(), pin.artifact_id)
    {
        let pin = pin.clone();
        let verified = match verified_revision_artifact::install_project_artifact_from_global_cache(
            &cached,
            install_lock,
            &wasm_path,
            lock_entry,
            name,
        ) {
            Ok(verified) => verified,
            Err(VerifiedArtifactError::ChecksumMismatch(mismatch)) => {
                let removal = revision_artifact_cache::remove(global_wasm.as_path());
                return Err((*mismatch).after_cache_removal(global_wasm.as_path(), removal));
            }
            Err(VerifiedArtifactError::Other(message)) => return Err(message),
        };
        if mode != LockMode::Locked {
            lock_file.upsert(revision_lock_entry(
                &verified,
                &pin.revision,
                &pin.resolved_sha,
            ));
        }
        eprintln!(
            "Installed provider '{}' from global cache ({}@{})",
            name,
            source,
            sha.short()
        );
        return Ok((verified.path().clone(), sha));
    }

    // 3. Download from CI artifacts
    eprintln!(
        "Downloading provider '{}' from CI artifacts ({source}@{})...",
        name,
        sha.short()
    );

    let selected = resolve_revision_artifact(
        &artifact_pin,
        RevisionArtifactLocation::new(source, &sha, base_dir),
        mode,
        || list_runs(sha.clone()),
        &mut list_artifacts,
        &mut artifact_head_sha,
        &mut download_artifact,
    )?;
    if let Some((_resolved_sha, expected_sha256)) = &legacy_pin {
        let actual_sha256 = format!("{:x}", Sha256::digest(selected.wasm_bytes()));
        if &actual_sha256 != expected_sha256 {
            return Err(format!(
                "Downloaded provider '{name}' ({source}@{sha}) does not match the sha256 pinned in the legacy carina-providers.lock entry. Expected: {expected_sha256}, got: {actual_sha256}."
            ));
        }
    }
    let unchanged_revision = artifact_pin.pinned().is_some();
    let published = if unchanged_revision {
        verified_revision_artifact::publish_unchanged_revision_download(
            &selected,
            install_lock,
            &wasm_path,
            artifact_pin,
            &source_key.to_string(),
            name,
        )
    } else {
        verified_revision_artifact::publish_download(
            &selected,
            install_lock,
            &wasm_path,
            artifact_pin,
            &source_key.to_string(),
            name,
        )
    }?;
    let verified = match published {
        PublishedRevisionArtifact::Pinned { artifact, pin } => {
            if mode != LockMode::Locked {
                lock_file.upsert(revision_lock_entry(
                    &artifact,
                    &pin.revision,
                    &pin.resolved_sha,
                ));
            }
            artifact
        }
        PublishedRevisionArtifact::FirstUse(artifact) => {
            if mode == LockMode::Locked {
                return Err(format!(
                    "Locked resolution for provider '{name}' has no matching revision entry"
                ));
            }
            lock_file.upsert(revision_lock_entry(&artifact, revision, &sha));
            artifact
        }
    };
    let installed_path = verified.path().clone();

    // Save to global cache
    if let Some(global_wasm) = global_cache_path_revision(source, &sha) {
        install_global_revision_artifact(
            installed_path.as_path(),
            &global_wasm,
            verified.artifact_id(),
        )?;
    }

    if let Some(recovery) = selected.recovery()
        && let Some(notice) =
            revision_artifact_recovery_notice(recovery, selected.artifact_id(), mode, base_dir)
    {
        eprintln!("{notice}");
    }

    eprintln!("Installed provider '{}' ({source}@{})", name, sha.short());

    Ok((installed_path, sha))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::{FixtureGithubHttp, GithubRequest};
    use crate::provider_resolver::{LockEntry, RegistryLock, sha256_file};
    use carina_core::provider_source::{ProviderSource, RemoteSource};
    use sha2::{Digest, Sha256};
    use std::cell::{Cell, RefCell};

    fn github_source(owner: &str, repo: &str) -> GithubSource {
        match format!("github.com/{owner}/{repo}")
            .parse::<ProviderSource>()
            .unwrap()
        {
            ProviderSource::Remote(RemoteSource::GithubDirect(source)) => source,
            source => panic!("expected GitHub source: {source}"),
        }
    }

    fn commit_sha(value: &str) -> CommitSha {
        CommitSha::new(value.to_string()).expect("test commit SHA must be canonical")
    }

    fn test_sha() -> CommitSha {
        commit_sha(TEST_SHA)
    }

    fn test_artifact_id(value: u64) -> ArtifactId {
        ArtifactId::from_lock(value)
    }

    fn revision_sidecar_path(wasm_path: &Path) -> PathBuf {
        let mut sidecar = wasm_path.as_os_str().to_os_string();
        sidecar.push(".artifact-id");
        PathBuf::from(sidecar)
    }

    fn write_revision_cache_metadata_for_test(wasm_path: &Path, artifact_id: ArtifactId) {
        revision_artifact_cache::publish(wasm_path, artifact_id, || Ok::<(), io::Error>(()))
            .unwrap();
    }

    fn workflow_run_id(value: u64) -> WorkflowRunId {
        serde_json::from_value(serde_json::json!(value)).unwrap()
    }

    #[test]
    fn invalid_config_revision_is_rejected_before_any_github_request() {
        let source = github_source("carina-rs", "provider");
        let http = FixtureGithubHttp::default();
        let api = GithubApi::new(&http, Some("token"));

        for revision in ["%2e%2e", "..", "/", "?", "#", "%2F", " ", ""] {
            let error = resolve_ref_to_sha_with_api(&source, revision, &api).unwrap_err();
            assert!(
                error.contains("invalid GitHub git ref"),
                "{revision}: {error}"
            );
        }
        assert!(http.requests().is_empty());
    }

    #[test]
    fn test_cache_path_revision() {
        let base = Path::new("/tmp/project");
        let sha = commit_sha("abc123def456789012345678901234567890abcd");
        let source = github_source("carina-rs", "carina-provider-awscc");
        let path = cache_path_revision(base, &source, &sha);
        assert_eq!(
            path,
            PathBuf::from(
                "/tmp/project/.carina/providers/github.com/carina-rs/carina-provider-awscc/rev-abc123def456/carina-provider-awscc.wasm"
            )
        );
    }

    #[test]
    fn commit_sha_short_returns_twelve_character_prefix() {
        assert_eq!(test_sha().short(), "abcdef012345");
    }

    #[test]
    fn checksum_mismatch_hint_uses_lock_derived_path_and_project_dir() {
        let dir = tempfile::tempdir().unwrap();
        let project_dir = dir.path().join("infra/foo");
        let source = test_source();
        let artifact_path = cache_path_revision(&project_dir, &source, &test_sha());
        std::fs::create_dir_all(artifact_path.parent().unwrap()).unwrap();
        std::fs::write(&artifact_path, b"actual").unwrap();
        let lock_entry = LockEntry {
            name: "aws".into(),
            source: TEST_SOURCE.into(),
            kind: crate::provider_resolver::LockEntryKind::Revision {
                revision: "main".into(),
                resolved_sha: test_sha(),
                artifact_id: test_artifact_id(42),
            },
            sha256: "expected".into(),
            registry: None::<RegistryLock>,
        };
        let nested = VerifiedArtifact::from_existing(&lock_entry, "aws", &project_dir)
            .unwrap_err()
            .to_string();
        assert!(
            nested.contains(&format!(
                "Run `carina init {}` to discard the mismatched copy, then run it again to download a verified one.",
                project_dir.display()
            )),
            "{nested}"
        );
    }

    #[test]
    fn cached_revision_checksum_mismatch_uses_resolver_project_dir() {
        let _env_guard = crate::test_env_guard();
        let _token = crate::ScopedEnvVar::set("GITHUB_TOKEN", "test-token");

        let dir = tempfile::tempdir().unwrap();
        let source = test_source();
        let wasm_path = cache_path_revision(dir.path(), &source, &test_sha());
        std::fs::create_dir_all(wasm_path.parent().unwrap()).unwrap();
        std::fs::write(&wasm_path, b"tampered provider").unwrap();
        write_revision_cache_metadata_for_test(&wasm_path, test_artifact_id(42));
        let sidecar_path = revision_sidecar_path(&wasm_path);
        let mut lock_file = crate::provider_resolver::LockFile::default();
        lock_file.upsert(crate::provider_resolver::LockEntry {
            name: "aws".into(),
            source: TEST_SOURCE.into(),
            kind: crate::provider_resolver::LockEntryKind::Revision {
                revision: "main".into(),
                resolved_sha: test_sha(),
                artifact_id: test_artifact_id(42),
            },
            sha256: "expected-sha256".into(),
            registry: None,
        });

        let install_lock = ProviderInstallLock::acquire(dir.path()).unwrap();
        let mut lock_file = install_lock.guard_lock_file_for_test(lock_file);
        let result =
            resolve_provider_by_revision(&mut lock_file, &source, "main", "aws", LockMode::Normal);
        let error = result.expect_err("the cached artifact hash must not match its lock pin");

        assert!(
            error.contains(&format!(
                "The mismatched cached copy was removed; re-run `carina init {}` to download it again.",
                dir.path().display()
            )),
            "{error}"
        );
        assert!(
            !wasm_path.exists(),
            "the mismatched project artifact must be removed before returning"
        );
        assert!(
            !sidecar_path.exists(),
            "the mismatched project's artifact-id sidecar must be removed too"
        );
    }

    #[test]
    fn fresh_revision_download_mismatch_is_rejected_before_publish() {
        let project = tempfile::tempdir().unwrap();
        let source = test_source();
        let destination = WasmArtifactPath::new(
            cache_path_revision(project.path(), &source, &test_sha()),
            "aws",
        )
        .unwrap();
        let install_lock = ProviderInstallLock::acquire(project.path()).unwrap();
        let mut staged = StagedArtifact::for_project(&install_lock, &destination).unwrap();
        staged
            .write_all(b"unexpected revision origin bytes")
            .unwrap();
        let mut lock_file = crate::provider_resolver::LockFile::default();
        lock_file.upsert(LockEntry {
            name: "aws".into(),
            source: TEST_SOURCE.into(),
            kind: crate::provider_resolver::LockEntryKind::Revision {
                revision: "main".into(),
                resolved_sha: test_sha(),
                artifact_id: test_artifact_id(42),
            },
            sha256: "locked-revision-sha256".into(),
            registry: None,
        });
        let pin = lookup_revision_artifact_pin(
            &lock_file,
            &source.source_key(),
            "main",
            &test_sha(),
            "aws",
        );

        let error = publish_download(staged, pin)
            .expect_err("fresh revision bytes must match an existing lock pin");

        assert!(error.contains("Downloaded provider 'aws'"), "{error}");
        assert!(error.contains("The origin artifact changed after it was pinned"));
        assert!(!destination.as_path().exists());
    }

    #[test]
    fn revision_global_cache_mismatch_is_not_published_or_repinned() {
        let _env_guard = crate::test_env_guard();
        let project = tempfile::tempdir().unwrap();
        let global = tempfile::tempdir().unwrap();
        let _token = crate::ScopedEnvVar::set("GITHUB_TOKEN", "test-token");
        let _cache = crate::ScopedEnvVar::set("CARINA_PLUGIN_CACHE_DIR", global.path());
        let source = test_source();
        let locked_bytes = b"locked revision provider";
        let cached_bytes = b"tampered global revision provider";
        let expected_file = project.path().join("expected.wasm");
        std::fs::write(&expected_file, locked_bytes).unwrap();
        let expected_sha256 = sha256_file(&expected_file).unwrap();
        let mut lock_file = crate::provider_resolver::LockFile::default();
        lock_file.upsert(crate::provider_resolver::LockEntry {
            name: "aws".into(),
            source: TEST_SOURCE.into(),
            kind: crate::provider_resolver::LockEntryKind::Revision {
                revision: "main".into(),
                resolved_sha: test_sha(),
                artifact_id: test_artifact_id(42),
            },
            sha256: expected_sha256.clone(),
            registry: None,
        });
        let lock_path = crate::provider_resolver::provider_lock_path(project.path());
        lock_file.save(&lock_path).unwrap();
        let lock_before = std::fs::read(&lock_path).unwrap();
        let global_wasm = global_cache_path_revision(&source, &test_sha()).unwrap();
        std::fs::create_dir_all(global_wasm.as_path().parent().unwrap()).unwrap();
        std::fs::write(global_wasm.as_path(), cached_bytes).unwrap();
        write_revision_cache_metadata_for_test(global_wasm.as_path(), test_artifact_id(42));
        let global_sidecar = revision_sidecar_path(global_wasm.as_path());
        let project_wasm = cache_path_revision(project.path(), &source, &test_sha());
        let actual_sha256 = sha256_file(global_wasm.as_path()).unwrap();
        let install_lock = ProviderInstallLock::acquire(project.path()).unwrap();
        let mut guarded = install_lock.guard_lock_file_for_test(lock_file);

        let result =
            resolve_provider_by_revision(&mut guarded, &source, "main", "aws", LockMode::Normal);

        let error = result.expect_err("a global cache entry must match the revision lock pin");
        assert!(
            error.contains("SHA256 mismatch for provider 'aws'"),
            "{error}"
        );
        assert!(error.contains(&expected_sha256), "{error}");
        assert!(error.contains(&actual_sha256), "{error}");
        assert!(
            !project_wasm.exists(),
            "mismatched bytes must not be published"
        );
        assert!(
            !global_wasm.as_path().exists(),
            "a mismatched global cache entry must be removed so the next init can download"
        );
        assert!(
            !global_sidecar.exists(),
            "a mismatched global cache entry's sidecar must be removed too"
        );
        assert_eq!(std::fs::read(&lock_path).unwrap(), lock_before);
        assert_eq!(
            guarded
                .find_by_source_key_and_sha(&source.source_key(), &test_sha())
                .unwrap()
                .sha256,
            expected_sha256
        );
    }

    #[test]
    fn revision_global_cache_match_installs_without_repinning() {
        let _env_guard = crate::test_env_guard();
        let project = tempfile::tempdir().unwrap();
        let global = tempfile::tempdir().unwrap();
        let _token = crate::ScopedEnvVar::set("GITHUB_TOKEN", "test-token");
        let _cache = crate::ScopedEnvVar::set("CARINA_PLUGIN_CACHE_DIR", global.path());
        let source = test_source();
        let cached_bytes = b"lock-matching global revision provider";
        let global_wasm = global_cache_path_revision(&source, &test_sha()).unwrap();
        std::fs::create_dir_all(global_wasm.as_path().parent().unwrap()).unwrap();
        std::fs::write(global_wasm.as_path(), cached_bytes).unwrap();
        let sha256 = sha256_file(global_wasm.as_path()).unwrap();
        let mut lock_file = crate::provider_resolver::LockFile::default();
        lock_file.upsert(crate::provider_resolver::LockEntry {
            name: "aws".into(),
            source: TEST_SOURCE.into(),
            kind: crate::provider_resolver::LockEntryKind::Revision {
                revision: "main".into(),
                resolved_sha: test_sha(),
                artifact_id: test_artifact_id(42),
            },
            sha256: sha256.clone(),
            registry: None,
        });
        let lock_path = crate::provider_resolver::provider_lock_path(project.path());
        lock_file.save(&lock_path).unwrap();
        let lock_before = std::fs::read(&lock_path).unwrap();
        let install_lock = ProviderInstallLock::acquire(project.path()).unwrap();
        let mut guarded = install_lock.guard_lock_file_for_test(lock_file);

        let (resolved, resolved_sha) =
            resolve_provider_by_revision(&mut guarded, &source, "main", "aws", LockMode::Normal)
                .unwrap();

        assert_eq!(resolved_sha, test_sha());
        assert_eq!(std::fs::read(resolved.as_path()).unwrap(), cached_bytes);
        assert_eq!(std::fs::read(&lock_path).unwrap(), lock_before);
        assert_eq!(
            guarded
                .find_by_source_key_and_sha(&source.source_key(), &test_sha())
                .unwrap()
                .sha256,
            sha256
        );
    }

    #[test]
    fn revision_global_sidecar_write_failure_does_not_replace_cached_bytes() {
        let _env_guard = crate::test_env_guard();
        let project = tempfile::tempdir().unwrap();
        let global = tempfile::tempdir().unwrap();
        let _cache = crate::ScopedEnvVar::set("CARINA_PLUGIN_CACHE_DIR", global.path());
        let source = test_source();
        let global_wasm = global_cache_path_revision(&source, &test_sha()).unwrap();
        std::fs::create_dir_all(global_wasm.as_path().parent().unwrap()).unwrap();
        let old_bytes = b"old globally cached provider";
        let new_bytes = b"newly selected provider";
        std::fs::write(global_wasm.as_path(), old_bytes).unwrap();

        let sidecar = revision_sidecar_path(global_wasm.as_path());
        std::fs::create_dir(&sidecar).unwrap();

        let install_lock = ProviderInstallLock::acquire(project.path()).unwrap();
        let mut guarded =
            install_lock.guard_lock_file_for_test(crate::provider_resolver::LockFile::default());
        let result = resolve_provider_by_revision_with(
            &mut guarded,
            &source,
            "main",
            "aws",
            LockMode::Normal,
            RevisionGithubOps {
                resolve_ref: || Ok(test_sha()),
                list_runs: |_: CommitSha| Ok(vec![workflow_run_id(7)]),
                list_artifacts: |_| Ok(vec![artifact("provider-build", 42, false)]),
                artifact_head_sha: |_| Ok(Some(test_sha())),
                download_artifact: |_| Ok(artifact_zip(&[("provider.wasm", new_bytes)])),
            },
        );

        assert!(
            result.is_err(),
            "a global revision sidecar failure must be reported"
        );
        assert_eq!(
            std::fs::read(global_wasm.as_path()).unwrap(),
            old_bytes,
            "new bytes must not be installed after their sidecar write fails"
        );
    }

    #[test]
    fn revision_project_sidecar_write_failure_after_recovery_does_not_publish_bytes() {
        let _env_guard = crate::test_env_guard();
        let project = tempfile::tempdir().unwrap();
        let global = tempfile::tempdir().unwrap();
        let _cache = crate::ScopedEnvVar::set("CARINA_PLUGIN_CACHE_DIR", global.path());
        let source = test_source();
        let wasm = b"locked provider";
        let mut lock_file = crate::provider_resolver::LockFile::default();
        lock_file.upsert(LockEntry {
            name: "aws".into(),
            source: TEST_SOURCE.into(),
            kind: crate::provider_resolver::LockEntryKind::Revision {
                revision: "main".into(),
                resolved_sha: test_sha(),
                artifact_id: test_artifact_id(42),
            },
            sha256: sha256_bytes(wasm),
            registry: None,
        });
        let wasm_path = cache_path_revision(project.path(), &source, &test_sha());
        let sidecar = revision_sidecar_path(&wasm_path);
        std::fs::create_dir_all(wasm_path.parent().unwrap()).unwrap();
        std::fs::create_dir(&sidecar).unwrap();
        let install_lock = ProviderInstallLock::acquire(project.path()).unwrap();
        let mut guarded = install_lock.guard_lock_file_for_test(lock_file);

        let error = resolve_provider_by_revision_with(
            &mut guarded,
            &source,
            "main",
            "aws",
            LockMode::Normal,
            RevisionGithubOps {
                resolve_ref: || panic!("the locked revision SHA must be reused"),
                list_runs: |_: CommitSha| Ok(vec![workflow_run_id(7)]),
                list_artifacts: |_| Ok(vec![artifact("replacement-build", 99, false)]),
                artifact_head_sha: |_| Ok(Some(test_sha())),
                download_artifact: |artifact_id: ArtifactId| match artifact_id.get() {
                    42 => Err(ArtifactDownloadError::missing_or_gone(404)),
                    99 => Ok(artifact_zip(&[("provider.wasm", wasm)])),
                    other => panic!("unexpected artifact {other}"),
                },
            },
        )
        .expect_err("the project revision sidecar failure must be reported");

        assert!(error.contains("revision cache metadata"), "{error}");
        assert!(
            !wasm_path.exists(),
            "replacement bytes must not be published with stale cache metadata"
        );
        assert!(matches!(
            guarded
                .find_by_source_key_and_sha(&source.source_key(), &test_sha())
                .unwrap()
                .kind,
            crate::provider_resolver::LockEntryKind::Revision { artifact_id, .. }
                if artifact_id.get() == 42
        ));
    }

    #[test]
    fn revision_cache_crash_state_is_reverified_before_use() {
        let _env_guard = crate::test_env_guard();
        let project = tempfile::tempdir().unwrap();
        let global = tempfile::tempdir().unwrap();
        let _cache = crate::ScopedEnvVar::set("CARINA_PLUGIN_CACHE_DIR", global.path());
        let source = test_source();
        let wasm_path = cache_path_revision(project.path(), &source, &test_sha());
        std::fs::create_dir_all(wasm_path.parent().unwrap()).unwrap();
        std::fs::write(&wasm_path, b"bytes left by the previous cache entry").unwrap();
        let publish_error = revision_artifact_cache::publish(
            &wasm_path,
            test_artifact_id(99),
            || -> Result<(), io::Error> {
                Err(io::Error::other("simulated byte publication failure"))
            },
        )
        .unwrap_err();
        assert!(matches!(
            publish_error,
            revision_artifact_cache::PublishError::Artifact(_)
        ));
        let sidecar = revision_sidecar_path(&wasm_path);
        assert_eq!(std::fs::read_to_string(&sidecar).unwrap().trim(), "99");
        let mut lock_file = crate::provider_resolver::LockFile::default();
        lock_file.upsert(LockEntry {
            name: "aws".into(),
            source: TEST_SOURCE.into(),
            kind: crate::provider_resolver::LockEntryKind::Revision {
                revision: "main".into(),
                resolved_sha: test_sha(),
                artifact_id: test_artifact_id(42),
            },
            sha256: sha256_bytes(b"locked provider"),
            registry: None,
        });
        let install_lock = ProviderInstallLock::acquire(project.path()).unwrap();
        let mut guarded = install_lock.guard_lock_file_for_test(lock_file);

        let error = resolve_provider_by_revision_with(
            &mut guarded,
            &source,
            "main",
            "aws",
            LockMode::Normal,
            RevisionGithubOps {
                resolve_ref: || panic!("the locked revision SHA must be reused"),
                list_runs: |_| panic!("an invalid warm cache must fail before origin recovery"),
                list_artifacts: |_| panic!("an invalid warm cache must not list artifacts"),
                artifact_head_sha: |_| panic!("an invalid warm cache must not inspect metadata"),
                download_artifact: |_| panic!("an invalid warm cache must not download"),
            },
        )
        .expect_err("the old bytes must still be checked against the sha256 pin");

        assert!(error.contains("SHA256 mismatch"), "{error}");
        assert!(!wasm_path.exists());
        assert!(!sidecar.exists());
    }

    #[test]
    fn test_get_github_token_from_env() {
        // This test only verifies the env var path; gh CLI path is harder to unit test
        let _env_guard = crate::test_env_guard();
        let _token = crate::ScopedEnvVar::set("GITHUB_TOKEN", "test-token-123");
        let token = get_github_token().unwrap();
        assert_eq!(token, "test-token-123");
    }

    const TEST_SOURCE: &str = "github.com/carina-rs/carina-provider-aws";
    const TEST_SHA: &str = "abcdef0123456789abcdef0123456789abcdef01";
    const OTHER_SHA: &str = "0123456789abcdef0123456789abcdef01234567";

    fn test_source() -> GithubSource {
        github_source("carina-rs", "carina-provider-aws")
    }

    fn artifact(name: &str, id: u64, expired: bool) -> Artifact {
        Artifact {
            id: ArtifactId::from_lock(id),
            name: name.to_string(),
            expired,
            size_in_bytes: None,
        }
    }

    fn workflow_run(
        id: u64,
        event: Option<&str>,
        head_repository: Option<&str>,
        head_sha: Option<&str>,
    ) -> GithubWorkflowRunResponse {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "event": event,
            "head_repository": head_repository.map(|full_name| serde_json::json!({
                "full_name": full_name,
            })),
            "head_sha": head_sha,
        }))
        .unwrap()
    }

    #[test]
    fn revision_workflow_run_filter_rejects_fork_pull_request_run() {
        let selection = filter_workflow_runs(
            &test_source(),
            &test_sha(),
            vec![workflow_run(
                101,
                Some("pull_request"),
                Some("contributor/carina-provider-aws"),
                Some(TEST_SHA),
            )],
        );

        assert!(selection.eligible_run_ids.is_empty());
        assert_eq!(selection.rejected_runs.len(), 1);
        assert_eq!(selection.rejected_runs[0].run_id.get(), 101);
        assert_eq!(
            selection.rejected_runs[0].reason,
            WorkflowRunRejectionReason::EventIsNotPush
        );
    }

    #[test]
    fn revision_workflow_run_filter_rejects_same_repo_pull_request_run() {
        let selection = filter_workflow_runs(
            &test_source(),
            &test_sha(),
            vec![workflow_run(
                102,
                Some("pull_request"),
                Some("carina-rs/carina-provider-aws"),
                Some(TEST_SHA),
            )],
        );

        assert!(selection.eligible_run_ids.is_empty());
        assert_eq!(selection.rejected_runs.len(), 1);
        assert_eq!(
            selection.rejected_runs[0].reason,
            WorkflowRunRejectionReason::EventIsNotPush
        );
    }

    #[test]
    fn revision_workflow_run_filter_accepts_matching_repository_push_run_case_insensitively() {
        let selection = filter_workflow_runs(
            &test_source(),
            &test_sha(),
            vec![workflow_run(
                103,
                Some("push"),
                Some("CARINA-RS/CARINA-PROVIDER-AWS"),
                Some(TEST_SHA),
            )],
        );

        assert_eq!(
            selection
                .eligible_run_ids
                .iter()
                .map(|run_id| run_id.get())
                .collect::<Vec<_>>(),
            vec![103]
        );
        assert!(selection.rejected_runs.is_empty());
    }

    #[test]
    fn revision_workflow_run_filter_rejects_other_repository_push_run() {
        let selection = filter_workflow_runs(
            &test_source(),
            &test_sha(),
            vec![workflow_run(
                104,
                Some("push"),
                Some("carina-rs/carina-provider-aws-fork"),
                Some(TEST_SHA),
            )],
        );

        assert!(selection.eligible_run_ids.is_empty());
        assert_eq!(selection.rejected_runs.len(), 1);
        assert_eq!(
            selection.rejected_runs[0].reason,
            WorkflowRunRejectionReason::HeadRepositoryMismatch
        );
    }

    #[test]
    fn revision_workflow_run_filter_rejects_different_head_sha() {
        let selection = filter_workflow_runs(
            &test_source(),
            &test_sha(),
            vec![workflow_run(
                105,
                Some("push"),
                Some("carina-rs/carina-provider-aws"),
                Some(OTHER_SHA),
            )],
        );

        assert!(selection.eligible_run_ids.is_empty());
        assert_eq!(selection.rejected_runs.len(), 1);
        assert_eq!(
            selection.rejected_runs[0].reason,
            WorkflowRunRejectionReason::HeadShaMismatch
        );
    }

    #[test]
    fn revision_workflow_run_filter_rejects_missing_head_sha() {
        let selection = filter_workflow_runs(
            &test_source(),
            &test_sha(),
            vec![workflow_run(
                106,
                Some("push"),
                Some("carina-rs/carina-provider-aws"),
                None,
            )],
        );

        assert!(selection.eligible_run_ids.is_empty());
        assert_eq!(selection.rejected_runs.len(), 1);
        assert_eq!(
            selection.rejected_runs[0].reason,
            WorkflowRunRejectionReason::MissingHeadSha
        );
    }

    #[test]
    fn revision_workflow_run_filter_mixed_runs_keeps_only_matching_run() {
        let selection = filter_workflow_runs(
            &test_source(),
            &test_sha(),
            vec![
                workflow_run(
                    107,
                    Some("pull_request"),
                    Some("contributor/carina-provider-aws"),
                    Some(TEST_SHA),
                ),
                workflow_run(
                    108,
                    Some("pull_request"),
                    Some("carina-rs/carina-provider-aws"),
                    Some(TEST_SHA),
                ),
                workflow_run(
                    109,
                    Some("push"),
                    Some("carina-rs/carina-provider-aws"),
                    Some(TEST_SHA),
                ),
                workflow_run(
                    110,
                    Some("push"),
                    Some("other/carina-provider-aws"),
                    Some(TEST_SHA),
                ),
                workflow_run(
                    111,
                    Some("push"),
                    Some("carina-rs/carina-provider-aws"),
                    Some(OTHER_SHA),
                ),
                workflow_run(
                    112,
                    Some("push"),
                    Some("carina-rs/carina-provider-aws"),
                    None,
                ),
            ],
        );

        assert_eq!(
            selection
                .eligible_run_ids
                .iter()
                .map(|run_id| run_id.get())
                .collect::<Vec<_>>(),
            vec![109]
        );
        assert_eq!(
            selection
                .rejected_runs
                .iter()
                .map(|rejection| rejection.run_id.get())
                .collect::<Vec<_>>(),
            vec![107, 108, 110, 111, 112]
        );
    }

    #[test]
    fn revision_workflow_run_filter_no_eligible_run_error_lists_rejections() {
        let source = test_source();
        let response: GithubWorkflowRunsResponse = serde_json::from_str(
            r#"{
                "workflow_runs": [
                    {
                        "id": 109,
                        "event": "pull_request",
                        "head_sha": "abcdef0123456789abcdef0123456789abcdef01",
                        "head_repository": {
                            "full_name": "contributor/carina-provider-aws"
                        }
                    },
                    {
                        "id": 110,
                        "event": "push",
                        "head_sha": "abcdef0123456789abcdef0123456789abcdef01",
                        "head_repository": {
                            "full_name": "other/carina-provider-aws"
                        }
                    },
                    {
                        "id": 111,
                        "head_sha": "abcdef0123456789abcdef0123456789abcdef01",
                        "head_repository": {
                            "full_name": "carina-rs/carina-provider-aws"
                        }
                    },
                    {
                        "id": 112,
                        "event": "push",
                        "head_sha": "abcdef0123456789abcdef0123456789abcdef01",
                        "head_repository": {}
                    }
                ]
            }"#,
        )
        .unwrap();
        let selection = filter_workflow_runs(&source, &test_sha(), response.workflow_runs);

        let error = selection
            .into_eligible_run_ids()
            .expect_err("all ineligible runs must produce an error")
            .to_string();

        assert!(
            error.contains("Only push runs from carina-rs/carina-provider-aws are used"),
            "{error}"
        );
        assert!(
            error.contains(
                "run 109 (event: pull_request, head repository: contributor/carina-provider-aws, head SHA: abcdef0123456789abcdef0123456789abcdef01)"
            ),
            "{error}"
        );
        assert!(
            error.contains(
                "run 110 (event: push, head repository: other/carina-provider-aws, head SHA: abcdef0123456789abcdef0123456789abcdef01)"
            ),
            "{error}"
        );
        assert!(
            error.contains(
                "run 111 (event: <missing>, head repository: carina-rs/carina-provider-aws, head SHA: abcdef0123456789abcdef0123456789abcdef01)"
            ),
            "{error}"
        );
        assert!(
            error.contains(
                "run 112 (event: push, head repository: <missing>, head SHA: abcdef0123456789abcdef0123456789abcdef01)"
            ),
            "{error}"
        );
    }

    fn artifact_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut writer = zip::ZipWriter::new(io::Cursor::new(Vec::new()));
        for (name, bytes) in entries {
            writer
                .start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            writer.write_all(bytes).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }

    fn set_central_directory_uncompressed_size(zip: &mut [u8], size: u32) {
        let header = zip
            .windows(4)
            .position(|window| window == b"PK\x01\x02")
            .expect("central directory header");
        zip[header + 24..header + 28].copy_from_slice(&size.to_le_bytes());
    }

    fn sha256_bytes(bytes: &[u8]) -> String {
        format!("{:x}", Sha256::digest(bytes))
    }

    fn assert_resolver_ignores_ineligible_workflow_run(
        rejected_head_repository: &str,
        rejected_head_sha: &str,
    ) {
        let _env_guard = crate::test_env_guard();
        let project = tempfile::tempdir().unwrap();
        let global = tempfile::tempdir().unwrap();
        let _cache = crate::ScopedEnvVar::set("CARINA_PLUGIN_CACHE_DIR", global.path());
        let source = test_source();
        let commit_url = "https://api.github.com/repos/carina-rs/carina-provider-aws/commits/main";
        let runs_url = format!(
            "https://api.github.com/repos/carina-rs/carina-provider-aws/actions/runs?head_sha={TEST_SHA}&status=success&event=push&per_page=100"
        );
        let eligible_artifacts_url = "https://api.github.com/repos/carina-rs/carina-provider-aws/actions/runs/7/artifacts?per_page=100";
        let rejected_artifacts_url = "https://api.github.com/repos/carina-rs/carina-provider-aws/actions/runs/8/artifacts?per_page=100";
        let eligible_zip_url =
            "https://api.github.com/repos/carina-rs/carina-provider-aws/actions/artifacts/42/zip";
        let rejected_zip_url =
            "https://api.github.com/repos/carina-rs/carina-provider-aws/actions/artifacts/99/zip";
        let http = FixtureGithubHttp::default()
            .json(commit_url, format!(r#"{{"sha":"{TEST_SHA}"}}"#))
            .json(
                &runs_url,
                format!(
                    r#"{{"workflow_runs":[
                        {{"id":7,"event":"push","head_repository":{{"full_name":"carina-rs/carina-provider-aws"}},"head_sha":"{TEST_SHA}"}},
                        {{"id":8,"event":"push","head_repository":{{"full_name":"{rejected_head_repository}"}},"head_sha":"{rejected_head_sha}"}}
                    ]}}"#
                ),
            )
            .json(
                eligible_artifacts_url,
                r#"{"artifacts":[{"id":42,"name":"eligible-build","expired":false}]}"#,
            )
            .json(
                rejected_artifacts_url,
                r#"{"artifacts":[{"id":99,"name":"rejected-build","expired":false}]}"#,
            )
            .bytes(
                eligible_zip_url,
                artifact_zip(&[("provider.wasm", b"eligible provider")]),
            )
            .bytes(
                rejected_zip_url,
                artifact_zip(&[("provider.wasm", b"ineligible provider")]),
            );
        let api = GithubApi::new(&http, Some("token"));
        let install_lock = ProviderInstallLock::acquire(project.path()).unwrap();
        let mut guarded =
            install_lock.guard_lock_file_for_test(crate::provider_resolver::LockFile::default());

        let (installed, resolved_sha) = resolve_provider_by_revision_with_api(
            &mut guarded,
            &source,
            "main",
            "aws",
            LockMode::Normal,
            &api,
        )
        .expect("only the eligible workflow run's build should be selected");

        assert_eq!(resolved_sha, test_sha());
        assert_eq!(
            std::fs::read(installed.as_path()).unwrap(),
            b"eligible provider"
        );
        assert!(matches!(
            guarded
                .find_by_source_key_and_sha(&source.source_key(), &test_sha())
                .unwrap()
                .kind,
            crate::provider_resolver::LockEntryKind::Revision { artifact_id, .. }
                if artifact_id.get() == 42
        ));
        assert!(
            http.requests()
                .iter()
                .all(|request| request.url() != rejected_artifacts_url),
            "the ineligible run must be filtered before artifact inspection"
        );
    }

    #[test]
    fn revision_resolver_filters_other_head_repository_before_artifact_selection() {
        assert_resolver_ignores_ineligible_workflow_run("other/carina-provider-aws", TEST_SHA);
    }

    #[test]
    fn revision_resolver_filters_other_head_sha_before_artifact_selection() {
        assert_resolver_ignores_ineligible_workflow_run("carina-rs/carina-provider-aws", OTHER_SHA);
    }

    #[test]
    fn normal_mode_reselects_legacy_revision_at_locked_sha_and_records_artifact_id() {
        let _env_guard = crate::test_env_guard();
        let project = tempfile::tempdir().unwrap();
        let global = tempfile::tempdir().unwrap();
        let _cache = crate::ScopedEnvVar::set("CARINA_PLUGIN_CACHE_DIR", global.path());
        let wasm = b"legacy pinned provider";
        let lock_path = crate::provider_resolver::provider_lock_path(project.path());
        std::fs::write(
            &lock_path,
            format!(
                r#"version = {}

[[provider]]
name = "aws"
source = "{TEST_SOURCE}"
mode = "revision"
revision = "main"
resolved_sha = "{TEST_SHA}"
sha256 = "{}"
"#,
                crate::provider_resolver::LockFile::CURRENT_VERSION,
                sha256_bytes(wasm)
            ),
        )
        .unwrap();
        let install_lock = ProviderInstallLock::acquire(project.path()).unwrap();
        let mut lock_file = install_lock.load_lock_file().unwrap().unwrap();
        let listed_shas = RefCell::new(Vec::new());

        let (installed, resolved_sha) = resolve_provider_by_revision_with(
            &mut lock_file,
            &test_source(),
            "main",
            "aws",
            LockMode::Normal,
            RevisionGithubOps {
                resolve_ref: || panic!("legacy migration must not re-resolve the revision ref"),
                list_runs: |sha: CommitSha| {
                    listed_shas.borrow_mut().push(sha);
                    Ok(vec![workflow_run_id(7)])
                },
                list_artifacts: |_| Ok(vec![artifact("provider-build", 42, false)]),
                artifact_head_sha: |_| Ok(Some(test_sha())),
                download_artifact: |_| Ok(artifact_zip(&[("provider.wasm", wasm)])),
            },
        )
        .unwrap();

        assert_eq!(resolved_sha, test_sha());
        assert_eq!(listed_shas.into_inner(), vec![test_sha()]);
        assert_eq!(std::fs::read(installed.as_path()).unwrap(), wasm);
        let entry = lock_file
            .find_by_source_key_and_sha(&test_source().source_key(), &test_sha())
            .expect("legacy entry must be replaced with a typed revision pin");
        assert!(matches!(
            &entry.kind,
            crate::provider_resolver::LockEntryKind::Revision {
                revision,
                resolved_sha,
                artifact_id,
            } if revision == "main"
                && resolved_sha == &test_sha()
                && artifact_id.get() == 42
        ));

        lock_file.save().unwrap();
        let saved = std::fs::read_to_string(lock_path).unwrap();
        assert!(saved.contains("artifact_id = 42"), "{saved}");
        assert_eq!(saved.matches("[[provider]]").count(), 1, "{saved}");
    }

    #[test]
    fn upgrade_mode_rejects_changed_content_at_legacy_locked_sha() {
        let _env_guard = crate::test_env_guard();
        let project = tempfile::tempdir().unwrap();
        let global = tempfile::tempdir().unwrap();
        let _cache = crate::ScopedEnvVar::set("CARINA_PLUGIN_CACHE_DIR", global.path());
        let locked_wasm = b"legacy pinned provider";
        let lock_path = crate::provider_resolver::provider_lock_path(project.path());
        std::fs::write(
            &lock_path,
            format!(
                r#"version = {}

[[provider]]
name = "aws"
source = "{TEST_SOURCE}"
mode = "revision"
revision = "main"
resolved_sha = "{TEST_SHA}"
sha256 = "{}"
"#,
                crate::provider_resolver::LockFile::CURRENT_VERSION,
                sha256_bytes(locked_wasm)
            ),
        )
        .unwrap();
        let install_lock = ProviderInstallLock::acquire(project.path()).unwrap();
        let mut lock_file = install_lock.load_lock_file().unwrap().unwrap();
        let resolved_ref = Cell::new(false);

        let error = resolve_provider_by_revision_with(
            &mut lock_file,
            &test_source(),
            "main",
            "aws",
            LockMode::Upgrade,
            RevisionGithubOps {
                resolve_ref: || {
                    resolved_ref.set(true);
                    Ok(test_sha())
                },
                list_runs: |_: CommitSha| Ok(vec![workflow_run_id(7)]),
                list_artifacts: |_| Ok(vec![artifact("provider-build", 42, false)]),
                artifact_head_sha: |_| Ok(Some(test_sha())),
                download_artifact: |_| {
                    Ok(artifact_zip(&[(
                        "provider.wasm",
                        b"changed provider" as &[u8],
                    )]))
                },
            },
        )
        .expect_err("upgrade must retain a legacy content pin while the commit is unchanged");

        assert!(
            resolved_ref.get(),
            "upgrade must re-resolve the revision ref"
        );
        assert!(
            error.contains(
                "does not match the sha256 pinned in the legacy carina-providers.lock entry"
            ),
            "{error}"
        );
        assert!(!cache_path_revision(project.path(), &test_source(), &test_sha()).exists());
    }

    #[test]
    fn revision_artifact_selection_uses_zip_contents_not_artifact_name() {
        let wasm = b"provider wasm";
        let selected = select_artifact_from_runs(
            &[workflow_run_id(100)],
            &test_source(),
            &test_sha(),
            Path::new("."),
            RevisionArtifactPinState::Unpinned,
            |_| Ok(vec![artifact("build-output", 42, false)]),
            |artifact_id| {
                assert_eq!(artifact_id.get(), 42);
                Ok(artifact_zip(&[("dist/provider.wasm", wasm)]))
            },
        )
        .unwrap();

        assert_eq!(selected.artifact_id().get(), 42);
        assert_eq!(selected.wasm_bytes(), wasm);
    }

    #[test]
    fn revision_artifact_selection_scans_all_runs_and_artifacts() {
        let selected = select_artifact_from_runs(
            &[workflow_run_id(200), workflow_run_id(100)],
            &test_source(),
            &test_sha(),
            Path::new("."),
            RevisionArtifactPinState::Unpinned,
            |run_id| match run_id.get() {
                200 => Ok(vec![artifact("docs", 1, false)]),
                100 => Ok(vec![
                    artifact("metadata", 2, false),
                    artifact("portable-build", 3, false),
                ]),
                other => panic!("unexpected run {other}"),
            },
            |artifact_id| match artifact_id.get() {
                1 => Ok(artifact_zip(&[("README.txt", b"docs")])),
                2 => Ok(artifact_zip(&[("metadata.json", b"{}")])),
                3 => Ok(artifact_zip(&[("out/provider.wasm", b"selected")])),
                other => panic!("unexpected artifact {other}"),
            },
        )
        .unwrap();

        assert_eq!(selected.artifact_id().get(), 3);
        assert_eq!(selected.wasm_bytes(), b"selected");
    }

    #[test]
    fn revision_artifact_selection_sees_conflicting_wasm_on_second_artifact_page() {
        let source = test_source();
        let repo = RepoSlug::try_from(&source).unwrap();
        let first = "https://api.github.com/repos/carina-rs/carina-provider-aws/actions/runs/7/artifacts?per_page=100";
        let second = "https://api.github.com/repos/carina-rs/carina-provider-aws/actions/runs/7/artifacts?per_page=100&page=2";
        let first_zip =
            "https://api.github.com/repos/carina-rs/carina-provider-aws/actions/artifacts/41/zip";
        let second_zip =
            "https://api.github.com/repos/carina-rs/carina-provider-aws/actions/artifacts/42/zip";
        let http = FixtureGithubHttp::default()
            .json_with_headers(
                first,
                r#"{"artifacts":[{"id":41,"name":"first-build","expired":false}]}"#,
                &[("Link", format!("<{second}>; rel=\"next\""))],
            )
            .json(
                second,
                r#"{"artifacts":[{"id":42,"name":"second-build","expired":false}]}"#,
            )
            .bytes(
                first_zip,
                artifact_zip(&[("provider.wasm", b"first provider")]),
            )
            .bytes(
                second_zip,
                artifact_zip(&[("provider.wasm", b"second provider")]),
            );
        let api = GithubApi::new(&http, Some("token"));

        let error = select_artifact_from_runs(
            &[workflow_run_id(7)],
            &source,
            &test_sha(),
            Path::new("."),
            RevisionArtifactPinState::Unpinned,
            |run_id| fetch_run_artifacts(&repo, run_id, &api),
            |artifact_id| download_artifact_zip(&repo, artifact_id, &api),
        )
        .expect_err("a conflicting .wasm on page 2 must make selection ambiguous");

        assert!(error.contains("first-build: provider.wasm"), "{error}");
        assert!(error.contains("second-build: provider.wasm"), "{error}");
        assert_eq!(
            http.requests()
                .iter()
                .map(GithubRequest::url)
                .collect::<Vec<_>>(),
            [first, second, first_zip, second_zip]
        );
    }

    #[test]
    fn revision_artifact_selection_no_wasm_lists_all_artifact_names() {
        let error = select_artifact_from_runs(
            &[workflow_run_id(100)],
            &test_source(),
            &test_sha(),
            Path::new("."),
            RevisionArtifactPinState::Unpinned,
            |_| {
                Ok(vec![
                    artifact("docs", 1, false),
                    artifact("metadata", 2, false),
                ])
            },
            |artifact_id| match artifact_id.get() {
                1 => Ok(artifact_zip(&[("README.md", b"docs")])),
                2 => Ok(artifact_zip(&[("metadata.json", b"{}")])),
                other => panic!("unexpected artifact {other}"),
            },
        )
        .unwrap_err();

        assert!(error.contains("docs"), "{error}");
        assert!(error.contains("metadata"), "{error}");
    }

    #[test]
    fn revision_artifact_selection_multiple_artifacts_lists_each_wasm() {
        let error = select_artifact_from_runs(
            &[workflow_run_id(100)],
            &test_source(),
            &test_sha(),
            Path::new("."),
            RevisionArtifactPinState::Unpinned,
            |_| {
                Ok(vec![
                    artifact("linux-build", 1, false),
                    artifact("macos-build", 2, false),
                ])
            },
            |artifact_id| match artifact_id.get() {
                1 => Ok(artifact_zip(&[("dist/linux.wasm", b"linux")])),
                2 => Ok(artifact_zip(&[("dist/macos.wasm", b"macos")])),
                other => panic!("unexpected artifact {other}"),
            },
        )
        .unwrap_err();

        assert!(error.contains("linux-build: dist/linux.wasm"), "{error}");
        assert!(error.contains("macos-build: dist/macos.wasm"), "{error}");
    }

    #[test]
    fn revision_artifact_selection_identical_wasm_uses_newest_run() {
        let wasm = b"same provider build";
        let selected = select_artifact_from_runs(
            &[workflow_run_id(200), workflow_run_id(100)],
            &test_source(),
            &test_sha(),
            Path::new("."),
            RevisionArtifactPinState::Unpinned,
            |run_id| match run_id.get() {
                200 => Ok(vec![artifact("newer-build", 2, false)]),
                100 => Ok(vec![artifact("older-build", 1, false)]),
                other => panic!("unexpected run {other}"),
            },
            |artifact_id| match artifact_id.get() {
                2 => Ok(artifact_zip(&[("dist/provider.wasm", wasm)])),
                1 => Ok(artifact_zip(&[("release/provider.wasm", wasm)])),
                other => panic!("unexpected artifact {other}"),
            },
        )
        .expect("identical WASM bytes are one candidate");

        assert_eq!(selected.artifact_id().get(), 2);
        assert_eq!(selected.wasm_bytes(), wasm);
    }

    #[test]
    fn revision_artifact_identical_wasm_entries_are_selectable_and_replayable() {
        let wasm = b"same provider build";
        let zip = artifact_zip(&[("dist/provider.wasm", wasm), ("pkg/provider.wasm", wasm)]);
        let selected = select_artifact_from_runs(
            &[workflow_run_id(100)],
            &test_source(),
            &test_sha(),
            Path::new("."),
            RevisionArtifactPinState::Unpinned,
            |_| Ok(vec![artifact("build-output", 42, false)]),
            |_| Ok(zip.clone()),
        )
        .expect("byte-identical entries are one distinct WASM content");

        let replayed = replay_locked_artifact(
            &test_source(),
            &test_sha(),
            test_artifact_id(42),
            |_| Ok(Some(test_sha())),
            |_| Ok(zip.clone()),
        )
        .expect("byte-identical entries must replay");

        assert_eq!(selected.artifact_id().get(), 42);
        assert_eq!(selected.wasm_bytes(), wasm);
        assert_eq!(replayed.artifact_id().get(), 42);
        assert_eq!(replayed.wasm_bytes(), wasm);
    }

    #[test]
    fn revision_artifact_selection_multiple_wasm_in_one_artifact_lists_each_entry() {
        let zip = artifact_zip(&[
            ("dist/provider.wasm", b"one"),
            ("fixtures/helper.wasm", b"two"),
        ]);
        let selection_error = select_artifact_from_runs(
            &[workflow_run_id(100)],
            &test_source(),
            &test_sha(),
            Path::new("."),
            RevisionArtifactPinState::Unpinned,
            |_| Ok(vec![artifact("build-output", 1, false)]),
            |_| Ok(zip.clone()),
        )
        .unwrap_err();

        assert!(
            selection_error.contains("build-output: dist/provider.wasm"),
            "{selection_error}"
        );
        assert!(
            selection_error.contains("build-output: fixtures/helper.wasm"),
            "{selection_error}"
        );

        let replay_error = match replay_locked_artifact(
            &test_source(),
            &test_sha(),
            test_artifact_id(1),
            |_| Ok(Some(test_sha())),
            |_| Ok(zip.clone()),
        )
        .expect_err("different WASM contents must not replay")
        {
            LockedReplayError::InvalidArtifact(message) => message,
            error => panic!("unexpected replay error: {error:?}"),
        };

        assert!(
            replay_error.contains("dist/provider.wasm"),
            "{replay_error}"
        );
        assert!(
            replay_error.contains("fixtures/helper.wasm"),
            "{replay_error}"
        );
    }

    #[test]
    fn revision_artifact_selection_rejects_oversized_listing_without_downloading() {
        let repo = RepoSlug::try_from(&test_source()).unwrap();
        let listing_url = "https://api.github.com/repos/carina-rs/carina-provider-aws/actions/runs/7/artifacts?per_page=100";
        let http = FixtureGithubHttp::default().json(
            listing_url,
            r#"{"artifacts":[{"id":42,"name":"huge-build","expired":false,"size_in_bytes":536870913}]}"#,
        );
        let api = GithubApi::new(&http, Some("token"));

        let error = select_artifact_from_runs(
            &[workflow_run_id(7)],
            &test_source(),
            &test_sha(),
            Path::new("."),
            RevisionArtifactPinState::Unpinned,
            |run_id| fetch_run_artifacts(&repo, run_id, &api),
            |artifact_id| download_artifact_zip(&repo, artifact_id, &api),
        )
        .unwrap_err();

        assert!(error.contains("huge-build"), "{error}");
        assert!(error.contains("536870913"), "{error}");
        assert!(error.contains("512 MiB"), "{error}");
        assert_eq!(
            http.requests()
                .iter()
                .map(GithubRequest::url)
                .collect::<Vec<_>>(),
            [listing_url]
        );
    }

    #[test]
    fn revision_artifact_selection_only_expired_artifacts_reports_expired() {
        let project_dir = Path::new("infra/project");
        let error = select_artifact_from_runs(
            &[workflow_run_id(100)],
            &test_source(),
            &test_sha(),
            project_dir,
            RevisionArtifactPinState::Pinned,
            |_| {
                Ok(vec![
                    artifact("first-build", 1, true),
                    artifact("second-build", 2, true),
                ])
            },
            |_| -> Result<Vec<u8>, ArtifactDownloadError> {
                panic!("expired artifacts must not be downloaded")
            },
        )
        .unwrap_err();

        assert!(error.contains("expired"), "{error}");
        assert!(error.contains("first-build"), "{error}");
        assert!(error.contains("second-build"), "{error}");
        assert_eq!(
            error,
            format!(
                "All artifacts in successful workflow runs for {}@{} have expired: first-build, second-build. Re-run CI for this commit or push a new commit (or point `revision` at one). If the rebuilt artifact differs from the pinned sha256, remove this provider's entry from carina-providers.lock and run `{}`.",
                test_source(),
                test_sha(),
                ProjectCommand::new("init", project_dir),
            )
        );
    }

    #[test]
    fn revision_artifact_first_use_expiry_remedy_does_not_claim_a_sha256_pin() {
        let lock_file = crate::provider_resolver::LockFile::default();
        let source = test_source();
        let pin = lookup_revision_artifact_pin(
            &lock_file,
            &source.source_key(),
            "main",
            &test_sha(),
            "aws",
        );

        let error = resolve_revision_artifact(
            &pin,
            RevisionArtifactLocation::new(&source, &test_sha(), Path::new("infra/project")),
            LockMode::Normal,
            || Ok(vec![workflow_run_id(100)]),
            |_| Ok(vec![artifact("expired-build", 1, true)]),
            |_| panic!("first-use selection must not inspect locked artifact metadata"),
            |_| -> Result<Vec<u8>, ArtifactDownloadError> {
                panic!("expired artifacts must not be downloaded")
            },
        )
        .unwrap_err();

        assert!(error.contains("expired-build"), "{error}");
        assert!(!error.contains("pinned sha256"), "{error}");
        assert!(error.contains("carina init infra/project"), "{error}");
    }

    #[test]
    fn candidate_download_error_has_one_download_prefix() {
        let source = test_source();
        let repo = RepoSlug::try_from(&source).unwrap();
        let url =
            "https://api.github.com/repos/carina-rs/carina-provider-aws/actions/artifacts/42/zip";
        let http = FixtureGithubHttp::default().response(
            url,
            500,
            r#"{"message":"upstream failure"}"#,
            &[],
        );
        let api = GithubApi::new(&http, Some("token"));

        let error = select_artifact_from_runs(
            &[workflow_run_id(100)],
            &source,
            &test_sha(),
            Path::new("."),
            RevisionArtifactPinState::Unpinned,
            |_| Ok(vec![artifact("provider-build", 42, false)]),
            |artifact_id| download_artifact_zip(&repo, artifact_id, &api),
        )
        .unwrap_err();

        assert_eq!(error.matches("Failed to download").count(), 1, "{error}");
    }

    #[test]
    fn revision_artifact_selection_no_wasm_lists_expired_artifacts_too() {
        let error = select_artifact_from_runs(
            &[workflow_run_id(100)],
            &test_source(),
            &test_sha(),
            Path::new("."),
            RevisionArtifactPinState::Unpinned,
            |_| {
                Ok(vec![
                    artifact("docs", 1, false),
                    artifact("provider-build", 2, true),
                ])
            },
            |artifact_id| match artifact_id.get() {
                1 => Ok(artifact_zip(&[("README.md", b"docs")])),
                other => panic!("expired artifact {other} must not be downloaded"),
            },
        )
        .unwrap_err();

        assert!(error.contains("docs"), "{error}");
        assert!(error.contains("provider-build"), "{error}");
        assert!(error.contains("expired"), "{error}");
    }

    #[test]
    fn revision_artifact_selection_rejects_declared_oversized_wasm_entry() {
        let mut zip = artifact_zip(&[("dist/provider.wasm", b"small")]);
        set_central_directory_uncompressed_size(&mut zip, 256 * 1024 * 1024 + 1);

        let error = select_artifact_from_runs(
            &[workflow_run_id(100)],
            &test_source(),
            &test_sha(),
            Path::new("."),
            RevisionArtifactPinState::Unpinned,
            |_| Ok(vec![artifact("provider-build", 1, false)]),
            |_| Ok(zip.clone()),
        )
        .unwrap_err();

        assert!(error.contains("provider-build"), "{error}");
        assert!(error.contains("dist/provider.wasm"), "{error}");
        assert!(error.contains("256 MiB"), "{error}");
    }

    #[test]
    fn wasm_entry_reader_caps_bytes_when_zip_underreports_size() {
        let mut zip = artifact_zip(&[("dist/provider.wasm", b"12345")]);
        set_central_directory_uncompressed_size(&mut zip, 1);

        let error = wasm_entries_with_limit(zip, 4).unwrap_err();

        assert!(error.contains("dist/provider.wasm"), "{error}");
        assert!(error.contains("exceeds"), "{error}");
    }

    #[test]
    fn revision_artifact_locked_replay_skips_run_listing() {
        let wasm = b"locked provider";
        let mut lock_file = crate::provider_resolver::LockFile::default();
        lock_file.upsert(LockEntry {
            name: "aws".into(),
            source: TEST_SOURCE.into(),
            kind: crate::provider_resolver::LockEntryKind::Revision {
                revision: "main".into(),
                resolved_sha: test_sha(),
                artifact_id: test_artifact_id(42),
            },
            sha256: sha256_bytes(wasm),
            registry: None,
        });
        let pin = lookup_revision_artifact_pin(
            &lock_file,
            &test_source().source_key(),
            "main",
            &test_sha(),
            "aws",
        );
        let listed_runs = Cell::new(false);

        let selected = resolve_revision_artifact(
            &pin,
            RevisionArtifactLocation::new(&test_source(), &test_sha(), Path::new(".")),
            LockMode::Normal,
            || {
                listed_runs.set(true);
                Ok(vec![workflow_run_id(100)])
            },
            |_| panic!("locked replay must not fetch run artifacts"),
            |_| Ok(Some(test_sha())),
            |artifact_id| {
                assert_eq!(artifact_id.get(), 42);
                Ok(artifact_zip(&[("provider.wasm", wasm)]))
            },
        )
        .unwrap();

        assert!(!listed_runs.get());
        assert_eq!(selected.artifact_id().get(), 42);
        assert_eq!(selected.wasm_bytes(), wasm);
    }

    #[test]
    fn revision_artifact_locked_replay_without_head_sha_uses_pinned_download() {
        let wasm = b"locked provider";
        let mut lock_file = crate::provider_resolver::LockFile::default();
        lock_file.upsert(LockEntry {
            name: "aws".into(),
            source: TEST_SOURCE.into(),
            kind: crate::provider_resolver::LockEntryKind::Revision {
                revision: "main".into(),
                resolved_sha: test_sha(),
                artifact_id: test_artifact_id(42),
            },
            sha256: sha256_bytes(wasm),
            registry: None,
        });
        let pin = lookup_revision_artifact_pin(
            &lock_file,
            &test_source().source_key(),
            "main",
            &test_sha(),
            "aws",
        );

        let selected = resolve_revision_artifact(
            &pin,
            RevisionArtifactLocation::new(&test_source(), &test_sha(), Path::new(".")),
            LockMode::Normal,
            || -> Result<Vec<WorkflowRunId>, String> {
                panic!("missing replay metadata must not force run discovery")
            },
            |_| panic!("missing replay metadata must not list candidate artifacts"),
            |_| Ok(None),
            |artifact_id| {
                assert_eq!(artifact_id.get(), 42);
                Ok(artifact_zip(&[("provider.wasm", wasm)]))
            },
        )
        .expect("the sha256 pin is sufficient when artifact head_sha is absent");

        assert_eq!(selected.artifact_id().get(), 42);
        assert_eq!(selected.wasm_bytes(), wasm);
        assert!(selected.recovery().is_none());
    }

    fn assert_revision_artifact_recovery_mode(mode: LockMode, expected_artifact_id: u64) {
        let _env_guard = crate::test_env_guard();
        let project = tempfile::tempdir().unwrap();
        let global = tempfile::tempdir().unwrap();
        let _cache = crate::ScopedEnvVar::set("CARINA_PLUGIN_CACHE_DIR", global.path());
        let source = test_source();
        let wasm = b"locked provider";
        let locked_sha256 = sha256_bytes(wasm);
        let mut lock_file = crate::provider_resolver::LockFile::default();
        lock_file.upsert(LockEntry {
            name: "aws".into(),
            source: TEST_SOURCE.into(),
            kind: crate::provider_resolver::LockEntryKind::Revision {
                revision: "main".into(),
                resolved_sha: test_sha(),
                artifact_id: test_artifact_id(42),
            },
            sha256: locked_sha256.clone(),
            registry: None,
        });
        let lock_path = crate::provider_resolver::provider_lock_path(project.path());
        let lock_model_before = lock_file.to_toml_string().unwrap();
        lock_file.save(&lock_path).unwrap();
        let lock_before = std::fs::read(&lock_path).unwrap();
        let install_lock = ProviderInstallLock::acquire(project.path()).unwrap();
        let mut guarded = install_lock.load_lock_file().unwrap().unwrap();

        let (installed, resolved_sha) = resolve_provider_by_revision_with(
            &mut guarded,
            &source,
            "main",
            "aws",
            mode,
            RevisionGithubOps {
                resolve_ref: || panic!("a locked revision SHA must be reused"),
                list_runs: |_: CommitSha| Ok(vec![workflow_run_id(100)]),
                list_artifacts: |_| Ok(vec![artifact("replacement-build", 99, false)]),
                artifact_head_sha: |_| Ok(Some(test_sha())),
                download_artifact: |artifact_id: ArtifactId| match artifact_id.get() {
                    42 => Err(ArtifactDownloadError::missing_or_gone(404)),
                    99 => Ok(artifact_zip(&[("provider.wasm", wasm)])),
                    other => panic!("unexpected artifact {other}"),
                },
            },
        )
        .expect("matching content should recover an unavailable pin");

        assert_eq!(resolved_sha, test_sha());
        assert_eq!(std::fs::read(installed.as_path()).unwrap(), wasm);
        let entry = guarded
            .find_by_source_key_and_sha(&source.source_key(), &test_sha())
            .unwrap();
        assert_eq!(entry.sha256, locked_sha256);
        assert!(matches!(
            entry.kind,
            crate::provider_resolver::LockEntryKind::Revision {
                artifact_id,
                ..
            } if artifact_id.get() == expected_artifact_id
        ));

        guarded.save().unwrap();
        if mode == LockMode::Locked {
            assert_eq!(guarded.to_toml_string().unwrap(), lock_model_before);
            assert_eq!(std::fs::read(&lock_path).unwrap(), lock_before);
        }
        let persisted = install_lock.load_lock_file().unwrap().unwrap();
        let persisted_entry = persisted
            .find_by_source_key_and_sha(&source.source_key(), &test_sha())
            .unwrap();
        assert_eq!(persisted_entry.sha256, locked_sha256);
        assert!(matches!(
            persisted_entry.kind,
            crate::provider_resolver::LockEntryKind::Revision {
                artifact_id,
                ..
            } if artifact_id.get() == expected_artifact_id
        ));
    }

    #[test]
    fn revision_artifact_recovery_locked_mode_installs_without_updating_lock() {
        assert_revision_artifact_recovery_mode(LockMode::Locked, 42);
    }

    #[test]
    fn revision_artifact_recovery_normal_mode_updates_artifact_id() {
        assert_revision_artifact_recovery_mode(LockMode::Normal, 99);
    }

    fn assert_locked_recovery_then_warm_cache_updates_artifact_id(global_cache_only: bool) {
        let _env_guard = crate::test_env_guard();
        let project = tempfile::tempdir().unwrap();
        let global = tempfile::tempdir().unwrap();
        let _cache = crate::ScopedEnvVar::set("CARINA_PLUGIN_CACHE_DIR", global.path());
        let source = test_source();
        let wasm = b"locked provider";
        let mut lock_file = crate::provider_resolver::LockFile::default();
        lock_file.upsert(LockEntry {
            name: "aws".into(),
            source: TEST_SOURCE.into(),
            kind: crate::provider_resolver::LockEntryKind::Revision {
                revision: "main".into(),
                resolved_sha: test_sha(),
                artifact_id: test_artifact_id(42),
            },
            sha256: sha256_bytes(wasm),
            registry: None,
        });
        let install_lock = ProviderInstallLock::acquire(project.path()).unwrap();
        let mut guarded = install_lock.guard_lock_file_for_test(lock_file);

        resolve_provider_by_revision_with(
            &mut guarded,
            &source,
            "main",
            "aws",
            LockMode::Locked,
            RevisionGithubOps {
                resolve_ref: || panic!("locked mode must reuse the pinned commit"),
                list_runs: |_: CommitSha| Ok(vec![workflow_run_id(100)]),
                list_artifacts: |_| Ok(vec![artifact("replacement-build", 99, false)]),
                artifact_head_sha: |_| Ok(Some(test_sha())),
                download_artifact: |artifact_id: ArtifactId| match artifact_id.get() {
                    42 => Err(ArtifactDownloadError::missing_or_gone(404)),
                    99 => Ok(artifact_zip(&[("provider.wasm", wasm)])),
                    other => panic!("unexpected artifact {other}"),
                },
            },
        )
        .unwrap();

        assert!(matches!(
            guarded
                .find_by_source_key_and_sha(&source.source_key(), &test_sha())
                .unwrap()
                .kind,
            crate::provider_resolver::LockEntryKind::Revision { artifact_id, .. }
                if artifact_id.get() == 42
        ));

        if global_cache_only {
            std::fs::remove_dir_all(project.path().join(".carina/providers")).unwrap();
        }

        resolve_provider_by_revision_with(
            &mut guarded,
            &source,
            "main",
            "aws",
            LockMode::Normal,
            RevisionGithubOps {
                resolve_ref: || panic!("normal mode must reuse the pinned commit"),
                list_runs: |_: CommitSha| panic!("a warm cache must skip run listing"),
                list_artifacts: |_| panic!("a warm cache must skip artifact listing"),
                artifact_head_sha: |_| panic!("a warm cache must skip artifact metadata"),
                download_artifact: |_| panic!("a warm cache must skip artifact download"),
            },
        )
        .unwrap();

        assert!(matches!(
            guarded
                .find_by_source_key_and_sha(&source.source_key(), &test_sha())
                .unwrap()
                .kind,
            crate::provider_resolver::LockEntryKind::Revision { artifact_id, .. }
                if artifact_id.get() == 99
        ));
    }

    #[test]
    fn locked_recovery_then_normal_init_repins_from_project_cache() {
        assert_locked_recovery_then_warm_cache_updates_artifact_id(false);
    }

    #[test]
    fn locked_recovery_then_normal_init_repins_from_global_cache() {
        assert_locked_recovery_then_warm_cache_updates_artifact_id(true);
    }

    #[test]
    fn revision_artifact_recovery_notice_describes_the_lock_policy_on_one_line() {
        let project_dir = Path::new("infra/project");
        let recovery = RevisionArtifactRecovery::unavailable(test_artifact_id(42));
        let locked = revision_artifact_recovery_notice(
            &recovery,
            test_artifact_id(99),
            LockMode::Locked,
            project_dir,
        )
        .unwrap();
        assert_eq!(
            locked,
            format!(
                "Pinned artifact 42 was unavailable; the artifact with the same sha256 was \
                 re-selected (99). Running `{}` updates the lock.",
                ProjectCommand::new("init", project_dir)
            )
        );
        assert!(
            locked.contains(&project_dir.display().to_string()),
            "{locked}"
        );
        assert!(!locked.contains('\n'));

        let normal = revision_artifact_recovery_notice(
            &recovery,
            test_artifact_id(99),
            LockMode::Normal,
            project_dir,
        )
        .unwrap();
        assert_eq!(
            normal,
            "Pinned artifact 42 was unavailable; the artifact with the same sha256 was \
             re-selected (99). The lock was updated."
        );
        assert!(!normal.contains('\n'));
        assert_eq!(
            revision_artifact_recovery_notice(
                &recovery,
                test_artifact_id(99),
                LockMode::Upgrade,
                project_dir,
            )
            .unwrap(),
            normal
        );
    }

    #[test]
    fn revision_artifact_recovery_notice_keeps_the_different_commit_reason() {
        let recovery = RevisionArtifactRecovery::different_commit(
            test_artifact_id(42),
            &commit_sha(OTHER_SHA),
            &test_sha(),
        );

        let notice = revision_artifact_recovery_notice(
            &recovery,
            test_artifact_id(99),
            LockMode::Normal,
            Path::new("."),
        )
        .unwrap();

        assert!(
            notice.contains(&format!("artifact 42 belongs to commit {OTHER_SHA}")),
            "{notice}"
        );
        assert!(notice.contains(TEST_SHA), "{notice}");
        assert!(notice.ends_with("The lock was updated."), "{notice}");
    }

    #[test]
    fn revision_artifact_recovery_notice_omits_same_artifact_reselection() {
        let recovery = RevisionArtifactRecovery::unavailable(test_artifact_id(42));

        assert!(
            revision_artifact_recovery_notice(
                &recovery,
                test_artifact_id(42),
                LockMode::Normal,
                Path::new("."),
            )
            .is_none(),
            "the same artifact ID must produce neither a notice nor a lock-update report"
        );
    }

    #[test]
    fn revision_artifact_missing_locked_artifact_recovers_by_content() {
        let wasm = b"locked provider";
        for status in [404, 410] {
            let mut lock_file = crate::provider_resolver::LockFile::default();
            lock_file.upsert(LockEntry {
                name: "aws".into(),
                source: TEST_SOURCE.into(),
                kind: crate::provider_resolver::LockEntryKind::Revision {
                    revision: "main".into(),
                    resolved_sha: test_sha(),
                    artifact_id: test_artifact_id(42),
                },
                sha256: sha256_bytes(wasm),
                registry: None,
            });
            let pin = lookup_revision_artifact_pin(
                &lock_file,
                &test_source().source_key(),
                "main",
                &test_sha(),
                "aws",
            );
            let downloads = RefCell::new(Vec::new());

            let selected = resolve_revision_artifact(
                &pin,
                RevisionArtifactLocation::new(&test_source(), &test_sha(), Path::new(".")),
                LockMode::Normal,
                || Ok(vec![workflow_run_id(100)]),
                |_| Ok(vec![artifact("replacement-build", 99, false)]),
                |_| Ok(Some(test_sha())),
                |artifact_id| {
                    downloads.borrow_mut().push(artifact_id);
                    match artifact_id.get() {
                        42 => Err(ArtifactDownloadError::missing_or_gone(status)),
                        99 => Ok(artifact_zip(&[("provider.wasm", wasm)])),
                        other => panic!("unexpected artifact {other}"),
                    }
                },
            )
            .expect("a missing pinned artifact should be recovered by matching content");

            assert_eq!(
                downloads
                    .into_inner()
                    .into_iter()
                    .map(ArtifactId::get)
                    .collect::<Vec<_>>(),
                vec![42, 99]
            );
            assert_eq!(selected.artifact_id().get(), 99);
            assert_eq!(selected.wasm_bytes(), wasm);

            let project = tempfile::tempdir().unwrap();
            let source = test_source();
            let destination = WasmArtifactPath::new(
                cache_path_revision(project.path(), &source, &test_sha()),
                "aws",
            )
            .unwrap();
            let install_lock = ProviderInstallLock::acquire(project.path()).unwrap();
            let (verified, verified_pin) = match verified_revision_artifact::publish_download(
                &selected,
                &install_lock,
                &destination,
                pin,
                TEST_SOURCE,
                "aws",
            )
            .unwrap()
            {
                PublishedRevisionArtifact::Pinned { artifact, pin } => (artifact, pin),
                PublishedRevisionArtifact::FirstUse(_) => {
                    panic!("recovery must retain the existing hash pin")
                }
            };
            let mut guarded = install_lock.guard_lock_file_for_test(lock_file);
            guarded.upsert(revision_lock_entry(
                &verified,
                &verified_pin.revision,
                &verified_pin.resolved_sha,
            ));
            let recorded = guarded
                .find_by_source_key_and_sha(&source.source_key(), &test_sha())
                .unwrap();
            assert_eq!(recorded.sha256, sha256_bytes(wasm));
            assert!(matches!(
                &recorded.kind,
                crate::provider_resolver::LockEntryKind::Revision {
                    artifact_id,
                    ..
                } if artifact_id.get() == 99
            ));
        }
    }

    #[test]
    fn revision_artifact_missing_metadata_recovers_without_downloading_the_pin() {
        let wasm = b"locked provider";
        let mut lock_file = crate::provider_resolver::LockFile::default();
        lock_file.upsert(LockEntry {
            name: "aws".into(),
            source: TEST_SOURCE.into(),
            kind: crate::provider_resolver::LockEntryKind::Revision {
                revision: "main".into(),
                resolved_sha: test_sha(),
                artifact_id: test_artifact_id(42),
            },
            sha256: sha256_bytes(wasm),
            registry: None,
        });
        let pin = lookup_revision_artifact_pin(
            &lock_file,
            &test_source().source_key(),
            "main",
            &test_sha(),
            "aws",
        );

        let selected = resolve_revision_artifact(
            &pin,
            RevisionArtifactLocation::new(&test_source(), &test_sha(), Path::new(".")),
            LockMode::Normal,
            || Ok(vec![workflow_run_id(100)]),
            |_| Ok(vec![artifact("replacement-build", 99, false)]),
            |artifact_id| {
                assert_eq!(artifact_id.get(), 42);
                Err(ArtifactDownloadError::missing_or_gone(404))
            },
            |artifact_id| {
                assert_eq!(artifact_id.get(), 99);
                Ok(artifact_zip(&[("provider.wasm", wasm)]))
            },
        )
        .expect("missing locked-artifact metadata should recover by pinned-commit content");

        assert_eq!(selected.artifact_id().get(), 99);
        assert_eq!(selected.recovery().unwrap().artifact_id().get(), 42);
    }

    #[test]
    fn revision_artifact_metadata_transport_error_fails_closed() {
        let wasm = b"locked provider";
        let mut lock_file = crate::provider_resolver::LockFile::default();
        lock_file.upsert(LockEntry {
            name: "aws".into(),
            source: TEST_SOURCE.into(),
            kind: crate::provider_resolver::LockEntryKind::Revision {
                revision: "main".into(),
                resolved_sha: test_sha(),
                artifact_id: test_artifact_id(42),
            },
            sha256: sha256_bytes(wasm),
            registry: None,
        });
        let pin = lookup_revision_artifact_pin(
            &lock_file,
            &test_source().source_key(),
            "main",
            &test_sha(),
            "aws",
        );

        let error = resolve_revision_artifact(
            &pin,
            RevisionArtifactLocation::new(&test_source(), &test_sha(), Path::new(".")),
            LockMode::Normal,
            || -> Result<Vec<WorkflowRunId>, String> {
                panic!("metadata failure must not enter recovery")
            },
            |_| panic!("metadata failure must not list candidate artifacts"),
            |_| {
                Err(ArtifactDownloadError::Other(
                    "simulated metadata transport failure".to_string(),
                ))
            },
            |_| panic!("metadata failure must not download the locked artifact"),
        )
        .unwrap_err();

        assert!(
            error.contains("simulated metadata transport failure"),
            "{error}"
        );
        assert!(error.contains("Retry"), "{error}");
    }

    #[test]
    fn revision_artifact_different_commit_recovery_carries_reason_to_notice() {
        let wasm = b"locked provider";
        let mut lock_file = crate::provider_resolver::LockFile::default();
        lock_file.upsert(LockEntry {
            name: "aws".into(),
            source: TEST_SOURCE.into(),
            kind: crate::provider_resolver::LockEntryKind::Revision {
                revision: "main".into(),
                resolved_sha: test_sha(),
                artifact_id: test_artifact_id(42),
            },
            sha256: sha256_bytes(wasm),
            registry: None,
        });
        let pin = lookup_revision_artifact_pin(
            &lock_file,
            &test_source().source_key(),
            "main",
            &test_sha(),
            "aws",
        );

        let selected = resolve_revision_artifact(
            &pin,
            RevisionArtifactLocation::new(&test_source(), &test_sha(), Path::new(".")),
            LockMode::Normal,
            || Ok(vec![workflow_run_id(7)]),
            |_| Ok(vec![artifact("replacement-build", 99, false)]),
            |_| Ok(Some(commit_sha(OTHER_SHA))),
            |artifact_id| {
                assert_eq!(artifact_id.get(), 99);
                Ok(artifact_zip(&[("provider.wasm", wasm)]))
            },
        )
        .expect("a cross-commit artifact ID should recover by pinned content");
        let recovery = selected.recovery().unwrap();
        let notice = revision_artifact_recovery_notice(
            recovery,
            selected.artifact_id(),
            LockMode::Normal,
            Path::new("."),
        )
        .unwrap();

        assert!(notice.contains(OTHER_SHA), "{notice}");
        assert!(notice.contains(TEST_SHA), "{notice}");
    }

    #[test]
    fn revision_artifact_same_id_recovery_has_no_notice_or_lock_update_report() {
        let wasm = b"locked provider";
        let mut lock_file = crate::provider_resolver::LockFile::default();
        lock_file.upsert(LockEntry {
            name: "aws".into(),
            source: TEST_SOURCE.into(),
            kind: crate::provider_resolver::LockEntryKind::Revision {
                revision: "main".into(),
                resolved_sha: test_sha(),
                artifact_id: test_artifact_id(42),
            },
            sha256: sha256_bytes(wasm),
            registry: None,
        });
        let pin = lookup_revision_artifact_pin(
            &lock_file,
            &test_source().source_key(),
            "main",
            &test_sha(),
            "aws",
        );
        let download_count = Cell::new(0);

        let selected = resolve_revision_artifact(
            &pin,
            RevisionArtifactLocation::new(&test_source(), &test_sha(), Path::new(".")),
            LockMode::Normal,
            || Ok(vec![workflow_run_id(7)]),
            |_| Ok(vec![artifact("same-build", 42, false)]),
            |_| Ok(Some(test_sha())),
            |_| {
                let call = download_count.get();
                download_count.set(call + 1);
                if call == 0 {
                    Err(ArtifactDownloadError::missing_or_gone(404))
                } else {
                    Ok(artifact_zip(&[("provider.wasm", wasm)]))
                }
            },
        )
        .expect("the same artifact can be rediscovered after a transient missing response");

        assert_eq!(download_count.get(), 2);
        assert_eq!(selected.artifact_id().get(), 42);
        assert!(
            revision_artifact_recovery_notice(
                selected.recovery().unwrap(),
                selected.artifact_id(),
                LockMode::Normal,
                Path::new("."),
            )
            .is_none()
        );
    }

    #[test]
    fn revision_artifact_from_another_commit_recovers_before_downloading_the_pin() {
        let _env_guard = crate::test_env_guard();
        let project = tempfile::tempdir().unwrap();
        let global = tempfile::tempdir().unwrap();
        let _cache = crate::ScopedEnvVar::set("CARINA_PLUGIN_CACHE_DIR", global.path());
        let source = test_source();
        let wasm = b"locked provider";
        let mut lock_file = crate::provider_resolver::LockFile::default();
        lock_file.upsert(LockEntry {
            name: "aws".into(),
            source: TEST_SOURCE.into(),
            kind: crate::provider_resolver::LockEntryKind::Revision {
                revision: "main".into(),
                resolved_sha: test_sha(),
                artifact_id: test_artifact_id(42),
            },
            sha256: sha256_bytes(wasm),
            registry: None,
        });

        let metadata_url =
            "https://api.github.com/repos/carina-rs/carina-provider-aws/actions/artifacts/42";
        let locked_zip_url =
            "https://api.github.com/repos/carina-rs/carina-provider-aws/actions/artifacts/42/zip";
        let runs_url = format!(
            "https://api.github.com/repos/carina-rs/carina-provider-aws/actions/runs?head_sha={TEST_SHA}&status=success&event=push&per_page=100"
        );
        let artifacts_url = "https://api.github.com/repos/carina-rs/carina-provider-aws/actions/runs/7/artifacts?per_page=100";
        let replacement_zip_url =
            "https://api.github.com/repos/carina-rs/carina-provider-aws/actions/artifacts/99/zip";
        let http = FixtureGithubHttp::default()
            .json(
                metadata_url,
                format!(
                    r#"{{"id":42,"name":"wrong-commit","expired":false,"workflow_run":{{"head_sha":"{OTHER_SHA}"}}}}"#
                ),
            )
            .bytes(
                locked_zip_url,
                artifact_zip(&[("provider.wasm", b"provider from another commit")]),
            )
            .json(
                &runs_url,
                format!(
                    r#"{{"workflow_runs":[{{"id":7,"event":"push","head_repository":{{"full_name":"carina-rs/carina-provider-aws"}},"head_sha":"{TEST_SHA}"}}]}}"#
                ),
            )
            .json(
                artifacts_url,
                r#"{"artifacts":[{"id":99,"name":"replacement-build","expired":false}]}"#,
            )
            .bytes(
                replacement_zip_url,
                artifact_zip(&[("provider.wasm", wasm)]),
            );
        let api = GithubApi::new(&http, Some("token"));
        let install_lock = ProviderInstallLock::acquire(project.path()).unwrap();
        let mut guarded = install_lock.guard_lock_file_for_test(lock_file);

        let (installed, resolved_sha) = resolve_provider_by_revision_with_api(
            &mut guarded,
            &source,
            "main",
            "aws",
            LockMode::Normal,
            &api,
        )
        .expect("an artifact ID from another commit must recover by hash at the pinned commit");

        assert_eq!(resolved_sha, test_sha());
        assert_eq!(std::fs::read(installed.as_path()).unwrap(), wasm);
        assert!(matches!(
            guarded
                .find_by_source_key_and_sha(&source.source_key(), &test_sha())
                .unwrap()
                .kind,
            crate::provider_resolver::LockEntryKind::Revision { artifact_id, .. }
                if artifact_id.get() == 99
        ));
        let requests = http.requests();
        assert!(requests.iter().any(|request| request.url() == metadata_url));
        assert!(
            requests
                .iter()
                .all(|request| request.url() != locked_zip_url),
            "the cross-commit artifact must not be downloaded"
        );
    }

    #[test]
    fn revision_artifact_missing_locked_artifact_rejects_different_content() {
        let mut lock_file = crate::provider_resolver::LockFile::default();
        lock_file.upsert(LockEntry {
            name: "aws".into(),
            source: TEST_SOURCE.into(),
            kind: crate::provider_resolver::LockEntryKind::Revision {
                revision: "main".into(),
                resolved_sha: test_sha(),
                artifact_id: test_artifact_id(42),
            },
            sha256: sha256_bytes(b"locked provider"),
            registry: None,
        });
        let source = test_source();
        let pin = lookup_revision_artifact_pin(
            &lock_file,
            &source.source_key(),
            "main",
            &test_sha(),
            "aws",
        );
        let selected = resolve_revision_artifact(
            &pin,
            RevisionArtifactLocation::new(&source, &test_sha(), Path::new(".")),
            LockMode::Normal,
            || Ok(vec![workflow_run_id(100)]),
            |_| Ok(vec![artifact("replacement-build", 99, false)]),
            |_| Ok(Some(test_sha())),
            |artifact_id| match artifact_id.get() {
                42 => Err(ArtifactDownloadError::missing_or_gone(404)),
                99 => Ok(artifact_zip(&[("provider.wasm", b"changed provider")])),
                other => panic!("unexpected artifact {other}"),
            },
        )
        .unwrap();
        let project = tempfile::tempdir().unwrap();
        let destination = WasmArtifactPath::new(
            cache_path_revision(project.path(), &source, &test_sha()),
            "aws",
        )
        .unwrap();
        let install_lock = ProviderInstallLock::acquire(project.path()).unwrap();
        let staged = stage_selected_artifact(&install_lock, &destination, &selected).unwrap();

        let error = publish_download(staged, pin)
            .expect_err("recovered content must match the existing hash pin");

        assert!(
            error.contains("does not match the sha256 pinned"),
            "{error}"
        );
        assert!(!destination.as_path().exists());
    }

    #[test]
    fn revision_artifact_expired_locked_artifact_recovers_by_content() {
        let wasm = b"locked provider";
        let mut lock_file = crate::provider_resolver::LockFile::default();
        lock_file.upsert(LockEntry {
            name: "aws".into(),
            source: TEST_SOURCE.into(),
            kind: crate::provider_resolver::LockEntryKind::Revision {
                revision: "main".into(),
                resolved_sha: test_sha(),
                artifact_id: test_artifact_id(42),
            },
            sha256: sha256_bytes(wasm),
            registry: None,
        });
        let pin = lookup_revision_artifact_pin(
            &lock_file,
            &test_source().source_key(),
            "main",
            &test_sha(),
            "aws",
        );

        let selected = resolve_revision_artifact(
            &pin,
            RevisionArtifactLocation::new(&test_source(), &test_sha(), Path::new(".")),
            LockMode::Normal,
            || Ok(vec![workflow_run_id(100)]),
            |_| {
                Ok(vec![
                    artifact("expired-build", 42, true),
                    artifact("replacement-build", 99, false),
                ])
            },
            |_| Ok(Some(test_sha())),
            |artifact_id| match artifact_id.get() {
                42 => Err(ArtifactDownloadError::Other(
                    "simulated non-status download failure".to_string(),
                )),
                99 => Ok(artifact_zip(&[("provider.wasm", wasm)])),
                other => panic!("unexpected artifact {other}"),
            },
        )
        .expect("metadata marking the pin expired should allow content recovery");

        assert_eq!(selected.artifact_id().get(), 99);
        assert_eq!(selected.wasm_bytes(), wasm);
    }

    #[test]
    fn revision_artifact_expired_pin_is_found_on_second_artifact_page() {
        let wasm = b"locked provider";
        let mut lock_file = crate::provider_resolver::LockFile::default();
        lock_file.upsert(LockEntry {
            name: "aws".into(),
            source: TEST_SOURCE.into(),
            kind: crate::provider_resolver::LockEntryKind::Revision {
                revision: "main".into(),
                resolved_sha: test_sha(),
                artifact_id: test_artifact_id(42),
            },
            sha256: sha256_bytes(wasm),
            registry: None,
        });
        let source = test_source();
        let repo = RepoSlug::try_from(&source).unwrap();
        let pin = lookup_revision_artifact_pin(
            &lock_file,
            &source.source_key(),
            "main",
            &test_sha(),
            "aws",
        );
        let first = "https://api.github.com/repos/carina-rs/carina-provider-aws/actions/runs/7/artifacts?per_page=100";
        let second = "https://api.github.com/repos/carina-rs/carina-provider-aws/actions/runs/7/artifacts?per_page=100&page=2";
        let http = FixtureGithubHttp::default()
            .json_with_headers(
                first,
                r#"{"artifacts":[{"id":99,"name":"replacement-build","expired":false}]}"#,
                &[("Link", format!("<{second}>; rel=\"next\""))],
            )
            .json(
                second,
                r#"{"artifacts":[{"id":42,"name":"expired-pin","expired":true}]}"#,
            );
        let api = GithubApi::new(&http, Some("token"));

        let selected = resolve_revision_artifact(
            &pin,
            RevisionArtifactLocation::new(&source, &test_sha(), Path::new(".")),
            LockMode::Normal,
            || Ok(vec![workflow_run_id(7)]),
            |run_id| fetch_run_artifacts(&repo, run_id, &api),
            |_| Ok(Some(test_sha())),
            |artifact_id| match artifact_id.get() {
                42 => Err(ArtifactDownloadError::Other(
                    "simulated transport failure".to_string(),
                )),
                99 => Ok(artifact_zip(&[("provider.wasm", wasm)])),
                other => panic!("unexpected artifact {other}"),
            },
        )
        .expect("an expired pin on page 2 must allow content recovery");

        assert_eq!(selected.artifact_id().get(), 99);
        assert_eq!(selected.recovery().unwrap().artifact_id().get(), 42);
        assert_eq!(
            http.requests()
                .iter()
                .map(GithubRequest::url)
                .collect::<Vec<_>>(),
            [first, second]
        );
    }

    #[test]
    fn revision_artifact_locked_download_failure_does_not_recommend_rerunning_ci() {
        let wasm = b"locked provider";
        let mut lock_file = crate::provider_resolver::LockFile::default();
        lock_file.upsert(LockEntry {
            name: "aws".into(),
            source: TEST_SOURCE.into(),
            kind: crate::provider_resolver::LockEntryKind::Revision {
                revision: "main".into(),
                resolved_sha: test_sha(),
                artifact_id: test_artifact_id(42),
            },
            sha256: sha256_bytes(wasm),
            registry: None,
        });
        let pin = lookup_revision_artifact_pin(
            &lock_file,
            &test_source().source_key(),
            "main",
            &test_sha(),
            "aws",
        );

        let error = resolve_revision_artifact(
            &pin,
            RevisionArtifactLocation::new(&test_source(), &test_sha(), Path::new(".")),
            LockMode::Normal,
            || Ok(vec![workflow_run_id(100)]),
            |_| Ok(vec![artifact("still-current", 42, false)]),
            |_| Ok(Some(test_sha())),
            |_| {
                Err(ArtifactDownloadError::Other(
                    "simulated transport failure".to_string(),
                ))
            },
        )
        .unwrap_err();

        assert!(error.contains("simulated transport failure"), "{error}");
        assert!(!error.contains("re-run CI"), "{error}");
    }

    #[test]
    fn locked_artifact_auth_error_has_no_retry_hint_or_doubled_prefix() {
        let source = test_source();
        let repo = RepoSlug::try_from(&source).unwrap();
        let url =
            "https://api.github.com/repos/carina-rs/carina-provider-aws/actions/artifacts/42/zip";

        for status in [401, 403] {
            let http = FixtureGithubHttp::default().response(
                url,
                status,
                r#"{"message":"access denied"}"#,
                &[],
            );
            let api = GithubApi::new(&http, Some("token"));
            let download_error =
                download_artifact_zip(&repo, test_artifact_id(42), &api).unwrap_err();
            let error = locked_artifact_error(
                LockedArtifactOperation::Download,
                &source,
                &test_sha(),
                test_artifact_id(42),
                &download_error,
            );

            assert_eq!(error.matches("Failed to download").count(), 1, "{error}");
            assert!(!error.contains("Retry"), "{error}");
        }
    }

    #[test]
    fn locked_artifact_rate_limit_responses_keep_the_retry_hint() {
        let source = test_source();
        let repo = RepoSlug::try_from(&source).unwrap();
        let url =
            "https://api.github.com/repos/carina-rs/carina-provider-aws/actions/artifacts/42/zip";
        let cases = [
            (403, vec![("x-ratelimit-remaining", "0".to_string())]),
            (403, vec![("retry-after", "60".to_string())]),
            (429, vec![("retry-after", "60".to_string())]),
        ];

        for (status, headers) in cases {
            let http = FixtureGithubHttp::default().response(
                url,
                status,
                r#"{"message":"API rate limit exceeded"}"#,
                &headers,
            );
            let api = GithubApi::new(&http, Some("token"));
            let download_error =
                download_artifact_zip(&repo, test_artifact_id(42), &api).unwrap_err();
            assert!(
                matches!(&download_error, ArtifactDownloadError::RateLimited(_)),
                "status {status}: {download_error}"
            );
            let error = locked_artifact_error(
                LockedArtifactOperation::Download,
                &source,
                &test_sha(),
                test_artifact_id(42),
                &download_error,
            );

            assert!(error.contains("Retry"), "status {status}: {error}");
        }
    }

    #[test]
    fn locked_artifact_body_size_error_has_no_retry_hint() {
        let download_error = ArtifactDownloadError::from_github(
            GithubApiError::ResponseBodyTooLarge {
                url: "https://api.github.com/repos/carina-rs/carina-provider-aws/actions/artifacts/42/zip"
                    .into(),
                limit: MAX_ARTIFACT_ZIP_COMPRESSED_SIZE,
            },
        );

        let error = locked_artifact_error(
            LockedArtifactOperation::Download,
            &test_source(),
            &test_sha(),
            test_artifact_id(42),
            &download_error,
        );

        assert!(error.contains("512 MiB"), "{error}");
        assert!(!error.contains("Retry"), "{error}");
    }

    #[test]
    fn revision_artifact_locked_replay_without_head_sha_rejects_sha256_mismatch() {
        let mut lock_file = crate::provider_resolver::LockFile::default();
        lock_file.upsert(LockEntry {
            name: "aws".into(),
            source: TEST_SOURCE.into(),
            kind: crate::provider_resolver::LockEntryKind::Revision {
                revision: "main".into(),
                resolved_sha: test_sha(),
                artifact_id: test_artifact_id(42),
            },
            sha256: sha256_bytes(b"locked provider"),
            registry: None,
        });
        let pin = lookup_revision_artifact_pin(
            &lock_file,
            &test_source().source_key(),
            "main",
            &test_sha(),
            "aws",
        );

        let selected = resolve_revision_artifact(
            &pin,
            RevisionArtifactLocation::new(&test_source(), &test_sha(), Path::new(".")),
            LockMode::Normal,
            || -> Result<Vec<WorkflowRunId>, String> { panic!("locked replay must not list runs") },
            |_| panic!("locked replay must not fetch run artifacts"),
            |_| Ok(None),
            |_| Ok(artifact_zip(&[("provider.wasm", b"changed provider")])),
        )
        .unwrap();
        let project = tempfile::tempdir().unwrap();
        let source = test_source();
        let destination = WasmArtifactPath::new(
            cache_path_revision(project.path(), &source, &test_sha()),
            "aws",
        )
        .unwrap();
        let install_lock = ProviderInstallLock::acquire(project.path()).unwrap();
        let staged = stage_selected_artifact(&install_lock, &destination, &selected).unwrap();
        let error = publish_download(staged, pin)
            .expect_err("locked replay must be hash checked at publish time");

        assert!(
            error.contains("does not match the sha256 pinned"),
            "{error}"
        );
        assert!(!destination.as_path().exists());
    }

    #[test]
    fn revision_artifact_upgrade_reselects_instead_of_replaying_pin() {
        let mut lock_file = crate::provider_resolver::LockFile::default();
        lock_file.upsert(LockEntry {
            name: "aws".into(),
            source: TEST_SOURCE.into(),
            kind: crate::provider_resolver::LockEntryKind::Revision {
                revision: "main".into(),
                resolved_sha: test_sha(),
                artifact_id: test_artifact_id(42),
            },
            sha256: sha256_bytes(b"new provider"),
            registry: None,
        });
        let pin = lookup_revision_artifact_pin(
            &lock_file,
            &test_source().source_key(),
            "main",
            &test_sha(),
            "aws",
        );
        let listed_runs = Cell::new(false);

        let selected = resolve_revision_artifact(
            &pin,
            RevisionArtifactLocation::new(&test_source(), &test_sha(), Path::new(".")),
            LockMode::Upgrade,
            || {
                listed_runs.set(true);
                Ok(vec![workflow_run_id(100)])
            },
            |_| Ok(vec![artifact("new-build", 99, false)]),
            |_| panic!("upgrade selection must not inspect locked artifact metadata"),
            |artifact_id| {
                assert_eq!(artifact_id.get(), 99);
                Ok(artifact_zip(&[("provider.wasm", b"new provider")]))
            },
        )
        .unwrap();

        assert!(listed_runs.get());
        assert_eq!(selected.artifact_id().get(), 99);
        assert_eq!(selected.wasm_bytes(), b"new provider");

        let project = tempfile::tempdir().unwrap();
        let source = test_source();
        let destination = WasmArtifactPath::new(
            cache_path_revision(project.path(), &source, &test_sha()),
            "aws",
        )
        .unwrap();
        let install_lock = ProviderInstallLock::acquire(project.path()).unwrap();
        let staged = stage_selected_artifact(&install_lock, &destination, &selected).unwrap();
        assert!(matches!(
            publish_unchanged_revision_download(staged, pin).unwrap(),
            DownloadedArtifact::Verified { .. }
        ));
    }

    #[test]
    fn revision_artifact_same_sha_upgrade_rejects_changed_content() {
        let project = tempfile::tempdir().unwrap();
        let source = test_source();
        let mut lock_file = crate::provider_resolver::LockFile::default();
        lock_file.upsert(LockEntry {
            name: "aws".into(),
            source: TEST_SOURCE.into(),
            kind: crate::provider_resolver::LockEntryKind::Revision {
                revision: "main".into(),
                resolved_sha: test_sha(),
                artifact_id: test_artifact_id(42),
            },
            sha256: sha256_bytes(b"old provider"),
            registry: None,
        });
        let pin = lookup_revision_artifact_pin(
            &lock_file,
            &source.source_key(),
            "main",
            &test_sha(),
            "aws",
        );
        let selected = select_artifact_from_runs(
            &[workflow_run_id(100)],
            &source,
            &test_sha(),
            project.path(),
            RevisionArtifactPinState::Pinned,
            |_| Ok(vec![artifact("rebuilt", 99, false)]),
            |_| Ok(artifact_zip(&[("provider.wasm", b"changed provider")])),
        )
        .unwrap();
        let destination = WasmArtifactPath::new(
            cache_path_revision(project.path(), &source, &test_sha()),
            "aws",
        )
        .unwrap();
        let install_lock = ProviderInstallLock::acquire(project.path()).unwrap();
        let staged = stage_selected_artifact(&install_lock, &destination, &selected).unwrap();

        let error = publish_unchanged_revision_download(staged, pin)
            .expect_err("an unchanged commit must retain its locked content hash");

        assert!(
            error.contains("artifact for an unchanged commit changed"),
            "{error}"
        );
        assert!(!destination.as_path().exists());
    }

    #[test]
    fn upgrade_same_sha_rejects_changed_content_through_revision_resolver() {
        let _env_guard = crate::test_env_guard();
        let project = tempfile::tempdir().unwrap();
        let global = tempfile::tempdir().unwrap();
        let _cache = crate::ScopedEnvVar::set("CARINA_PLUGIN_CACHE_DIR", global.path());
        let source = test_source();
        let mut lock_file = crate::provider_resolver::LockFile::default();
        lock_file.upsert(LockEntry {
            name: "aws".into(),
            source: TEST_SOURCE.into(),
            kind: crate::provider_resolver::LockEntryKind::Revision {
                revision: "main".into(),
                resolved_sha: test_sha(),
                artifact_id: test_artifact_id(42),
            },
            sha256: sha256_bytes(b"old provider"),
            registry: None,
        });
        let install_lock = ProviderInstallLock::acquire(project.path()).unwrap();
        let mut guarded = install_lock.guard_lock_file_for_test(lock_file);

        let error = resolve_provider_by_revision_with(
            &mut guarded,
            &source,
            "main",
            "aws",
            LockMode::Upgrade,
            RevisionGithubOps {
                resolve_ref: || Ok(test_sha()),
                list_runs: |_: CommitSha| Ok(vec![workflow_run_id(7)]),
                list_artifacts: |_| Ok(vec![artifact("rebuilt", 99, false)]),
                artifact_head_sha: |_| Ok(Some(test_sha())),
                download_artifact: |_| {
                    Ok(artifact_zip(&[(
                        "provider.wasm",
                        b"changed provider" as &[u8],
                    )]))
                },
            },
        )
        .expect_err("an unchanged commit must retain its locked content hash");

        assert!(
            error.contains("artifact for an unchanged commit changed"),
            "{error}"
        );
        assert!(!cache_path_revision(project.path(), &source, &test_sha()).exists());
    }

    #[test]
    fn normal_and_upgrade_unchanged_revision_mismatches_have_the_same_remedy() {
        for mode in [LockMode::Normal, LockMode::Upgrade] {
            let _env_guard = crate::test_env_guard();
            let project = tempfile::tempdir().unwrap();
            let global = tempfile::tempdir().unwrap();
            let _cache = crate::ScopedEnvVar::set("CARINA_PLUGIN_CACHE_DIR", global.path());
            let source = test_source();
            let mut lock_file = crate::provider_resolver::LockFile::default();
            lock_file.upsert(LockEntry {
                name: "aws".into(),
                source: TEST_SOURCE.into(),
                kind: crate::provider_resolver::LockEntryKind::Revision {
                    revision: "main".into(),
                    resolved_sha: test_sha(),
                    artifact_id: test_artifact_id(42),
                },
                sha256: sha256_bytes(b"old provider"),
                registry: None,
            });
            let install_lock = ProviderInstallLock::acquire(project.path()).unwrap();
            let mut guarded = install_lock.guard_lock_file_for_test(lock_file);

            let error = resolve_provider_by_revision_with(
                &mut guarded,
                &source,
                "main",
                "aws",
                mode,
                RevisionGithubOps {
                    resolve_ref: || Ok(test_sha()),
                    list_runs: |_: CommitSha| Ok(vec![workflow_run_id(7)]),
                    list_artifacts: |_| Ok(vec![artifact("rebuilt", 99, false)]),
                    artifact_head_sha: |_| Ok(Some(test_sha())),
                    download_artifact: |artifact_id: ArtifactId| match artifact_id.get() {
                        42 => Err(ArtifactDownloadError::missing_or_gone(404)),
                        99 => Ok(artifact_zip(&[(
                            "provider.wasm",
                            b"changed provider" as &[u8],
                        )])),
                        other => panic!("unexpected artifact {other}"),
                    },
                },
            )
            .expect_err("an unchanged commit must retain its locked content hash");
            let remedy = revision_artifact_replacement_remedy(project.path());

            assert!(error.ends_with(&remedy), "{mode:?}: {error}");
            assert!(!error.contains("refusing to replace"), "{mode:?}: {error}");
        }
    }

    #[test]
    fn upgrade_renamed_revision_at_same_sha_rejects_changed_content() {
        let _env_guard = crate::test_env_guard();
        let project = tempfile::tempdir().unwrap();
        let global = tempfile::tempdir().unwrap();
        let _cache = crate::ScopedEnvVar::set("CARINA_PLUGIN_CACHE_DIR", global.path());
        let source = test_source();
        let mut lock_file = crate::provider_resolver::LockFile::default();
        lock_file.upsert(LockEntry {
            name: "aws".into(),
            source: TEST_SOURCE.into(),
            kind: crate::provider_resolver::LockEntryKind::Revision {
                revision: "main".into(),
                resolved_sha: test_sha(),
                artifact_id: test_artifact_id(42),
            },
            sha256: sha256_bytes(b"old provider"),
            registry: None,
        });
        let install_lock = ProviderInstallLock::acquire(project.path()).unwrap();
        let mut guarded = install_lock.guard_lock_file_for_test(lock_file);

        let error = resolve_provider_by_revision_with(
            &mut guarded,
            &source,
            "v1.2.3",
            "aws",
            LockMode::Upgrade,
            RevisionGithubOps {
                resolve_ref: || Ok(test_sha()),
                list_runs: |_: CommitSha| Ok(vec![workflow_run_id(7)]),
                list_artifacts: |_| Ok(vec![artifact("rebuilt", 99, false)]),
                artifact_head_sha: |_| Ok(Some(test_sha())),
                download_artifact: |_| {
                    Ok(artifact_zip(&[(
                        "provider.wasm",
                        b"changed provider" as &[u8],
                    )]))
                },
            },
        )
        .expect_err("renaming a ref must not discard the commit's content pin");

        assert!(
            error.contains("artifact for an unchanged commit changed"),
            "{error}"
        );
        assert!(!cache_path_revision(project.path(), &source, &test_sha()).exists());
    }

    #[test]
    fn upgrade_renamed_revision_at_same_sha_records_new_revision_when_content_matches() {
        let _env_guard = crate::test_env_guard();
        let project = tempfile::tempdir().unwrap();
        let global = tempfile::tempdir().unwrap();
        let _cache = crate::ScopedEnvVar::set("CARINA_PLUGIN_CACHE_DIR", global.path());
        let source = test_source();
        let wasm = b"locked provider";
        let mut lock_file = crate::provider_resolver::LockFile::default();
        lock_file.upsert(LockEntry {
            name: "aws".into(),
            source: TEST_SOURCE.into(),
            kind: crate::provider_resolver::LockEntryKind::Revision {
                revision: "main".into(),
                resolved_sha: test_sha(),
                artifact_id: test_artifact_id(42),
            },
            sha256: sha256_bytes(wasm),
            registry: None,
        });
        let install_lock = ProviderInstallLock::acquire(project.path()).unwrap();
        let mut guarded = install_lock.guard_lock_file_for_test(lock_file);

        let (installed, resolved_sha) = resolve_provider_by_revision_with(
            &mut guarded,
            &source,
            "v1.2.3",
            "aws",
            LockMode::Upgrade,
            RevisionGithubOps {
                resolve_ref: || Ok(test_sha()),
                list_runs: |_: CommitSha| Ok(vec![workflow_run_id(7)]),
                list_artifacts: |_| Ok(vec![artifact("rebuilt", 99, false)]),
                artifact_head_sha: |_| Ok(Some(test_sha())),
                download_artifact: |_| Ok(artifact_zip(&[("provider.wasm", wasm)])),
            },
        )
        .unwrap();

        assert_eq!(resolved_sha, test_sha());
        assert_eq!(std::fs::read(installed.as_path()).unwrap(), wasm);
        let entry = guarded
            .find_by_source_key_and_sha(&source.source_key(), &test_sha())
            .unwrap();
        assert_eq!(entry.sha256, sha256_bytes(wasm));
        assert!(matches!(
            &entry.kind,
            crate::provider_resolver::LockEntryKind::Revision {
                revision,
                resolved_sha,
                artifact_id,
            } if revision == "v1.2.3"
                && resolved_sha == &test_sha()
                && artifact_id.get() == 99
        ));
    }

    #[test]
    fn upgrade_moved_sha_reselects_and_records_new_content() {
        let _env_guard = crate::test_env_guard();
        let project = tempfile::tempdir().unwrap();
        let global = tempfile::tempdir().unwrap();
        let _cache = crate::ScopedEnvVar::set("CARINA_PLUGIN_CACHE_DIR", global.path());
        let source = test_source();
        let moved_sha = commit_sha(OTHER_SHA);
        let moved_wasm = b"provider from moved commit";
        let mut lock_file = crate::provider_resolver::LockFile::default();
        lock_file.upsert(LockEntry {
            name: "aws".into(),
            source: TEST_SOURCE.into(),
            kind: crate::provider_resolver::LockEntryKind::Revision {
                revision: "main".into(),
                resolved_sha: test_sha(),
                artifact_id: test_artifact_id(42),
            },
            sha256: sha256_bytes(b"old provider"),
            registry: None,
        });
        let install_lock = ProviderInstallLock::acquire(project.path()).unwrap();
        let mut guarded = install_lock.guard_lock_file_for_test(lock_file);

        let (installed, resolved_sha) = resolve_provider_by_revision_with(
            &mut guarded,
            &source,
            "main",
            "aws",
            LockMode::Upgrade,
            RevisionGithubOps {
                resolve_ref: || Ok(moved_sha.clone()),
                list_runs: |sha: CommitSha| {
                    assert_eq!(sha, moved_sha);
                    Ok(vec![workflow_run_id(7)])
                },
                list_artifacts: |_| Ok(vec![artifact("moved-build", 99, false)]),
                artifact_head_sha: |_| Ok(Some(test_sha())),
                download_artifact: |_| Ok(artifact_zip(&[("provider.wasm", moved_wasm)])),
            },
        )
        .unwrap();

        assert_eq!(resolved_sha, moved_sha);
        assert_eq!(std::fs::read(installed.as_path()).unwrap(), moved_wasm);
        let entry = guarded
            .find_by_source_key_and_sha(&source.source_key(), &moved_sha)
            .unwrap();
        assert_eq!(entry.sha256, sha256_bytes(moved_wasm));
        assert!(matches!(
            &entry.kind,
            crate::provider_resolver::LockEntryKind::Revision {
                revision,
                resolved_sha,
                artifact_id,
            } if revision == "main"
                && resolved_sha == &moved_sha
                && artifact_id.get() == 99
        ));
    }

    #[test]
    fn test_artifact_deserialize() {
        // Sanity check the serde mapping against a realistic GitHub payload.
        let json = r#"{
            "artifacts": [
                {"id": 12345, "name": "carina-provider-aws.wasm", "expired": false,
                 "node_id": "ignored", "size_in_bytes": 1024}
            ]
        }"#;
        let url = "https://api.github.com/repos/carina-rs/carina-provider-aws/actions/runs/7/artifacts?per_page=100";
        let http = FixtureGithubHttp::default().json(url, json);
        let api = GithubApi::new(&http, Some("token"));
        let repo = RepoSlug::try_from(&test_source()).unwrap();
        let parsed = api.run_artifacts(&repo, workflow_run_id(7)).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].id.get(), 12345);
        assert_eq!(parsed[0].name, "carina-provider-aws.wasm");
        assert!(!parsed[0].expired);
        assert_eq!(parsed[0].size_in_bytes, Some(1024));
    }
}
