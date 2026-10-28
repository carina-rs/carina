//! Resolve provider binaries from GitHub Actions CI artifacts by git revision.

use serde::Deserialize;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use carina_core::provider_source::GithubSource;
use sha2::{Digest, Sha256};

use crate::install_lock::{
    GlobalArtifactPath, GuardedLockFile, ProviderInstallLock, StagedArtifact,
    global_cache_artifact_path, install_global_artifact_atomically,
    install_project_artifact_from_global_cache,
};
use crate::provider_resolver::WasmArtifactPath;
use crate::verified_artifact::{
    ArtifactPin, DownloadedArtifact, RevisionArtifactPin, VerifiedArtifact, VerifiedArtifactError,
    lookup_revision_artifact_pin, publish_download,
};

/// GitHub Actions artifact metadata (subset used by the resolver).
#[derive(Deserialize)]
struct Artifact {
    id: u64,
    name: String,
    #[serde(default)]
    expired: bool,
}

#[derive(Deserialize)]
struct ArtifactsResponse {
    #[serde(default)]
    artifacts: Vec<Artifact>,
}

#[derive(Deserialize)]
struct WorkflowRun {
    id: u64,
}

#[derive(Deserialize)]
struct WorkflowRunsResponse {
    #[serde(default)]
    workflow_runs: Vec<WorkflowRun>,
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
        .args(["auth", "token"])
        .output()
        .map_err(|e| {
            format!("Failed to run `gh auth token`: {e}. Set GITHUB_TOKEN or install gh CLI.")
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
) -> Result<String, String> {
    let owner = source.owner();
    let repo = source.repo();
    let url = format!("https://api.github.com/repos/{owner}/{repo}/commits/{revision}");

    let response = ureq::get(&url)
        .header("Accept", "application/vnd.github+json")
        .header("Authorization", &format!("Bearer {token}"))
        .header("User-Agent", "carina")
        .call()
        .map_err(|e| format!("Failed to resolve revision '{revision}' for {source}: {e}"))?;

    if response.status() != 200 {
        return Err(format!(
            "GitHub API returned status {} when resolving revision '{revision}' for {source}",
            response.status()
        ));
    }

    let body: String = response
        .into_body()
        .read_to_string()
        .map_err(|e| format!("Failed to read response body: {e}"))?;
    let commit: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| format!("Failed to parse response: {e}"))?;

    commit
        .get("sha")
        .and_then(|s| s.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| format!("No SHA found for revision '{revision}' in {source}"))
}

/// Find all successful workflow runs for a given commit SHA.
/// Returns run IDs newest-first, matching GitHub's default ordering.
///
/// Multiple workflows may run on the same commit (e.g. `ci.yml` and `docs.yml`
/// both triggered on push to main). The caller inspects artifacts from every
/// run to find the unique WASM entry — see
/// [`select_artifact_from_runs`].
fn find_workflow_run_ids(
    source: &GithubSource,
    sha: &str,
    token: &str,
) -> Result<Vec<u64>, String> {
    let owner = source.owner();
    let repo = source.repo();
    let url = format!(
        "https://api.github.com/repos/{owner}/{repo}/actions/runs?head_sha={sha}&status=success&per_page=10"
    );

    let response = ureq::get(&url)
        .header("Accept", "application/vnd.github+json")
        .header("Authorization", &format!("Bearer {token}"))
        .header("User-Agent", "carina")
        .call()
        .map_err(|e| format!("Failed to fetch workflow runs for {source}@{sha}: {e}"))?;

    if response.status() != 200 {
        return Err(format!(
            "GitHub API returned status {} when fetching workflow runs",
            response.status()
        ));
    }

    let body: String = response
        .into_body()
        .read_to_string()
        .map_err(|e| format!("Failed to read response body: {e}"))?;
    let data: WorkflowRunsResponse =
        serde_json::from_str(&body).map_err(|e| format!("Failed to parse response: {e}"))?;

    Ok(data.workflow_runs.into_iter().map(|r| r.id).collect())
}

/// Fetch the artifacts list for a single workflow run.
fn fetch_run_artifacts(
    source: &GithubSource,
    run_id: u64,
    token: &str,
) -> Result<Vec<Artifact>, String> {
    let owner = source.owner();
    let repo = source.repo();
    let url =
        format!("https://api.github.com/repos/{owner}/{repo}/actions/runs/{run_id}/artifacts");

    let response = ureq::get(&url)
        .header("Accept", "application/vnd.github+json")
        .header("Authorization", &format!("Bearer {token}"))
        .header("User-Agent", "carina")
        .call()
        .map_err(|e| format!("Failed to fetch artifacts for run {run_id}: {e}"))?;

    if response.status() != 200 {
        return Err(format!(
            "GitHub API returned status {} when fetching artifacts",
            response.status()
        ));
    }

    let body: String = response
        .into_body()
        .read_to_string()
        .map_err(|e| format!("Failed to read response body: {e}"))?;
    let data: ArtifactsResponse =
        serde_json::from_str(&body).map_err(|e| format!("Failed to parse response: {e}"))?;
    Ok(data.artifacts)
}

struct WasmZipEntry {
    name: String,
    bytes: Vec<u8>,
}

fn wasm_entries(zip_data: Vec<u8>) -> Result<Vec<WasmZipEntry>, String> {
    let cursor = io::Cursor::new(zip_data);
    let mut archive = zip::ZipArchive::new(cursor)
        .map_err(|error| format!("Failed to open artifact ZIP: {error}"))?;
    let mut entries = Vec::new();
    for index in 0..archive.len() {
        let mut file = archive
            .by_index(index)
            .map_err(|error| format!("Failed to read ZIP entry: {error}"))?;
        if file.is_dir() || !file.name().ends_with(".wasm") {
            continue;
        }
        let name = file.name().to_string();
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)
            .map_err(|error| format!("Failed to read WASM entry '{name}': {error}"))?;
        entries.push(WasmZipEntry { name, bytes });
    }
    Ok(entries)
}

mod revision_artifact_selection {
    use super::*;

    /// A GitHub artifact selected by inspecting its ZIP contents.
    ///
    /// Its fields and constructor stay private to this module, so an arbitrary
    /// artifact ID cannot acquire selected-artifact provenance.
    #[derive(Debug)]
    pub(super) struct SelectedRevisionArtifact {
        artifact_id: u64,
        wasm_bytes: Vec<u8>,
        sha256: String,
    }

    impl SelectedRevisionArtifact {
        fn new(artifact_id: u64, wasm_bytes: Vec<u8>) -> Self {
            let sha256 = format!("{:x}", Sha256::digest(&wasm_bytes));
            Self {
                artifact_id,
                wasm_bytes,
                sha256,
            }
        }

        pub(super) fn artifact_id(&self) -> u64 {
            self.artifact_id
        }

        pub(super) fn wasm_bytes(&self) -> &[u8] {
            &self.wasm_bytes
        }

        pub(super) fn sha256(&self) -> &str {
            &self.sha256
        }
    }

    struct CandidateWasm {
        artifact_id: u64,
        artifact_name: String,
        entry: WasmZipEntry,
    }

    pub(super) fn select_artifact_from_runs<F, D>(
        run_ids: &[u64],
        source: &GithubSource,
        sha: &str,
        mut fetch: F,
        mut download: D,
    ) -> Result<SelectedRevisionArtifact, String>
    where
        F: FnMut(u64) -> Result<Vec<Artifact>, String>,
        D: FnMut(u64) -> Result<Vec<u8>, String>,
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
                candidate_names.push(artifact.name.clone());
                let zip_data = download(artifact.id).map_err(|error| {
                    format!(
                        "Failed to download candidate artifact '{}' (id {}): {error}",
                        artifact.name, artifact.id
                    )
                })?;
                let entries = wasm_entries(zip_data).map_err(|error| {
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

        match wasm_candidates.len() {
            1 => {
                let candidate = wasm_candidates.pop().expect("length checked");
                Ok(SelectedRevisionArtifact::new(
                    candidate.artifact_id,
                    candidate.entry.bytes,
                ))
            }
            0 if candidate_names.is_empty() && !expired_names.is_empty() => Err(format!(
                "All artifacts in successful workflow runs for {source}@{sha} have expired: {}. \
                 Re-run CI for this revision or run `carina init --upgrade`.",
                expired_names.join(", ")
            )),
            0 => {
                let candidates = if candidate_names.is_empty() {
                    "(none)".to_string()
                } else {
                    candidate_names.join(", ")
                };
                Err(format!(
                    "No .wasm entry found in any non-expired artifact for {source}@{sha}. \
                     Candidate artifacts: {candidates}."
                ))
            }
            _ => {
                let causes = wasm_candidates
                    .iter()
                    .map(|candidate| {
                        format!("{}: {}", candidate.artifact_name, candidate.entry.name)
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                Err(format!(
                    "Multiple .wasm entries found across candidate artifacts for {source}@{sha}: \
                     {causes}. Exactly one .wasm entry is required."
                ))
            }
        }
    }

    pub(super) fn replay_locked_artifact<D>(
        source: &GithubSource,
        sha: &str,
        artifact_id: u64,
        expected_sha256: &str,
        mut download: D,
    ) -> Result<SelectedRevisionArtifact, String>
    where
        D: FnMut(u64) -> Result<Vec<u8>, String>,
    {
        let zip_data = download(artifact_id).map_err(|error| {
            format!(
                "Failed to download locked artifact {artifact_id} for {source}@{sha}: {error}. \
                 The artifact may be missing or expired; re-run CI for this revision or run \
                 `carina init --upgrade`."
            )
        })?;
        let mut entries = wasm_entries(zip_data).map_err(|error| {
            format!("Failed to inspect locked artifact {artifact_id} for {source}@{sha}: {error}")
        })?;
        if entries.len() != 1 {
            let names = entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            let names = if names.is_empty() { "(none)" } else { &names };
            return Err(format!(
                "Locked artifact {artifact_id} for {source}@{sha} must contain exactly one .wasm \
                 entry, found {}: {names}. Re-run CI for this revision or run \
                 `carina init --upgrade`.",
                entries.len()
            ));
        }
        let entry = entries.pop().expect("length checked");
        let selected = SelectedRevisionArtifact::new(artifact_id, entry.bytes);
        if selected.sha256() != expected_sha256 {
            return Err(format!(
                "SHA256 mismatch for locked artifact {artifact_id} ({source}@{sha}). \
                 Expected: {expected_sha256}, got: {}. The pinned artifact contents changed; \
                 if that change is intended, run `carina init --upgrade`.",
                selected.sha256()
            ));
        }
        Ok(selected)
    }
}

use revision_artifact_selection::{
    SelectedRevisionArtifact, replay_locked_artifact, select_artifact_from_runs,
};

fn resolve_revision_artifact<F, A, D>(
    artifact_pin: &ArtifactPin<'_, RevisionArtifactPin>,
    source: &GithubSource,
    sha: &str,
    mut find_runs: F,
    fetch: A,
    download: D,
) -> Result<SelectedRevisionArtifact, String>
where
    F: FnMut() -> Result<Vec<u64>, String>,
    A: FnMut(u64) -> Result<Vec<Artifact>, String>,
    D: FnMut(u64) -> Result<Vec<u8>, String>,
{
    if let Some((lock_entry, pin)) = artifact_pin.pinned() {
        return replay_locked_artifact(source, sha, pin.artifact_id, &lock_entry.sha256, download);
    }
    let run_ids = find_runs()?;
    select_artifact_from_runs(&run_ids, source, sha, fetch, download)
}

/// Download a GitHub Actions artifact ZIP.
fn download_artifact_zip(
    source: &GithubSource,
    artifact_id: u64,
    token: &str,
) -> Result<Vec<u8>, String> {
    let owner = source.owner();
    let repo = source.repo();
    let url =
        format!("https://api.github.com/repos/{owner}/{repo}/actions/artifacts/{artifact_id}/zip");

    let response = ureq::get(&url)
        .header("Accept", "application/vnd.github+json")
        .header("Authorization", &format!("Bearer {token}"))
        .header("User-Agent", "carina")
        .call()
        .map_err(|e| format!("Failed to download artifact {artifact_id}: {e}"))?;

    if response.status() != 200 {
        return Err(format!(
            "GitHub API returned status {} when downloading artifact",
            response.status()
        ));
    }

    let mut zip_data = Vec::new();
    response
        .into_body()
        .into_reader()
        .read_to_end(&mut zip_data)
        .map_err(|e| format!("Failed to read artifact data: {e}"))?;
    Ok(zip_data)
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
pub fn cache_path_revision(base_dir: &Path, source: &GithubSource, sha: &str) -> PathBuf {
    let sha_prefix = &sha[..sha.len().min(12)];
    base_dir
        .join(".carina")
        .join("providers")
        .join(source.to_string())
        .join(format!("rev-{sha_prefix}"))
        .join(format!("{}.wasm", source.repo()))
}

pub(crate) fn global_cache_path_revision(
    source: &GithubSource,
    sha: &str,
) -> Option<GlobalArtifactPath> {
    let sha_prefix = &sha[..sha.len().min(12)];
    global_cache_artifact_path(
        PathBuf::from(source.to_string())
            .join(format!("rev-{sha_prefix}"))
            .join(format!("{}.wasm", source.repo())),
    )
}

fn record_verified_revision_pin(
    lock_file: &mut GuardedLockFile<'_>,
    artifact: &VerifiedArtifact,
    pin: RevisionArtifactPin,
    name: &str,
) {
    lock_file.upsert(crate::provider_resolver::LockEntry {
        name: name.to_string(),
        source: artifact.source().to_string(),
        kind: crate::provider_resolver::LockEntryKind::Revision {
            revision: pin.revision,
            resolved_sha: pin.resolved_sha,
            artifact_id: pin.artifact_id,
        },
        sha256: artifact.sha256().to_string(),
        registry: None,
    });
}

/// Resolve a provider by revision: download from CI artifacts if not cached.
pub fn resolve_provider_by_revision(
    lock_file: &mut GuardedLockFile<'_>,
    source: &GithubSource,
    revision: &str,
    name: &str,
    upgrade: bool,
) -> Result<(WasmArtifactPath, String), String> {
    let install_lock = lock_file.install_lock();
    let base_dir = install_lock.project_dir();
    let token = get_github_token()?;
    let source_key = source.source_key();

    // 1. Resolve revision to SHA
    let sha = if !upgrade
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
    } else {
        eprintln!(
            "Resolving revision '{}' for provider '{}'...",
            revision, name
        );
        resolve_ref_to_sha(source, revision, &token)?
    };

    let artifact_pin =
        lookup_revision_artifact_pin(lock_file, &source_key, revision, &sha, name, !upgrade);

    // As in version mode, machine-local artifacts are never first-use trust
    // anchors. A project or global cache entry without this exact revision pin
    // is skipped in favor of a fresh origin download.
    let wasm_path = WasmArtifactPath::new(cache_path_revision(base_dir, source, &sha), name)?;
    if wasm_path.as_path().exists()
        && let Some((lock_entry, pin)) = artifact_pin.pinned()
    {
        let pin = pin.clone();
        let verified = match VerifiedArtifact::from_existing(lock_entry, name, base_dir) {
            Ok(verified) => verified,
            Err(VerifiedArtifactError::ChecksumMismatch(mismatch)) => {
                let removal = install_lock.remove_project_artifact(&wasm_path);
                return Err((*mismatch).after_cache_removal(wasm_path.as_path(), removal));
            }
            Err(VerifiedArtifactError::Other(message)) => return Err(message),
        };
        record_verified_revision_pin(lock_file, &verified, pin, name);
        return Ok((verified.path().clone(), sha));
    }

    if let Some((lock_entry, pin)) = artifact_pin.pinned()
        && let Some(global_wasm) = global_cache_path_revision(source, &sha)
        && global_wasm.as_path().exists()
    {
        let pin = pin.clone();
        let verified = match install_project_artifact_from_global_cache(
            install_lock,
            global_wasm.as_path(),
            &wasm_path,
            lock_entry,
            name,
        ) {
            Ok(verified) => verified,
            Err(VerifiedArtifactError::ChecksumMismatch(mismatch)) => {
                let removal = std::fs::remove_file(global_wasm.as_path());
                return Err((*mismatch).after_cache_removal(global_wasm.as_path(), removal));
            }
            Err(VerifiedArtifactError::Other(message)) => return Err(message),
        };
        record_verified_revision_pin(lock_file, &verified, pin, name);
        eprintln!(
            "Installed provider '{}' from global cache ({}@{})",
            name,
            source,
            &sha[..sha.len().min(12)]
        );
        return Ok((verified.path().clone(), sha));
    }

    // 3. Download from CI artifacts
    eprintln!(
        "Downloading provider '{}' from CI artifacts ({source}@{})...",
        name,
        &sha[..sha.len().min(12)]
    );

    let selected = resolve_revision_artifact(
        &artifact_pin,
        source,
        &sha,
        || find_workflow_run_ids(source, &sha, &token),
        |run_id| fetch_run_artifacts(source, run_id, &token),
        |artifact_id| download_artifact_zip(source, artifact_id, &token),
    )?;
    let artifact_id = selected.artifact_id();
    let selected_sha256 = selected.sha256().to_string();
    let staged = stage_selected_artifact(install_lock, &wasm_path, &selected)?;
    let installed_path = match publish_download(staged, artifact_pin)? {
        DownloadedArtifact::Verified { artifact, pin } => {
            record_verified_revision_pin(lock_file, &artifact, pin, name);
            artifact.path().clone()
        }
        DownloadedArtifact::FirstUse { path, sha256 } => {
            debug_assert_eq!(&sha256, &selected_sha256);
            lock_file.upsert(super::provider_resolver::LockEntry {
                name: name.to_string(),
                source: source_key.to_string(),
                kind: super::provider_resolver::LockEntryKind::Revision {
                    revision: revision.to_string(),
                    resolved_sha: sha.clone(),
                    artifact_id,
                },
                sha256: selected_sha256,
                registry: None,
            });
            path
        }
    };

    // Save to global cache
    if let Some(global_wasm) = global_cache_path_revision(source, &sha) {
        let _ = install_global_artifact_atomically(installed_path.as_path(), &global_wasm);
    }

    eprintln!(
        "Installed provider '{}' ({source}@{})",
        name,
        &sha[..sha.len().min(12)]
    );

    Ok((installed_path, sha))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider_resolver::{LockEntry, RegistryLock, sha256_file};
    use carina_core::provider_source::{ProviderSource, RemoteSource};
    use sha2::{Digest, Sha256};
    use std::cell::Cell;

    fn github_source(owner: &str, repo: &str) -> GithubSource {
        match format!("github.com/{owner}/{repo}")
            .parse::<ProviderSource>()
            .unwrap()
        {
            ProviderSource::Remote(RemoteSource::GithubDirect(source)) => source,
            source => panic!("expected GitHub source: {source}"),
        }
    }

    #[test]
    fn test_cache_path_revision() {
        let base = Path::new("/tmp/project");
        let sha = "abc123def456789012345678901234567890abcd";
        let source = github_source("carina-rs", "carina-provider-awscc");
        let path = cache_path_revision(base, &source, sha);
        assert_eq!(
            path,
            PathBuf::from(
                "/tmp/project/.carina/providers/github.com/carina-rs/carina-provider-awscc/rev-abc123def456/carina-provider-awscc.wasm"
            )
        );
    }

    #[test]
    fn test_cache_path_revision_short_sha() {
        let base = Path::new("/tmp/project");
        let sha = "abc123";
        let source = github_source("carina-rs", "carina-provider-awscc");
        let path = cache_path_revision(base, &source, sha);
        assert_eq!(
            path,
            PathBuf::from(
                "/tmp/project/.carina/providers/github.com/carina-rs/carina-provider-awscc/rev-abc123/carina-provider-awscc.wasm"
            )
        );
    }

    #[test]
    fn checksum_mismatch_hint_uses_lock_derived_path_and_project_dir() {
        let dir = tempfile::tempdir().unwrap();
        let project_dir = dir.path().join("infra/foo");
        let source = test_source();
        let artifact_path = cache_path_revision(&project_dir, &source, TEST_SHA);
        std::fs::create_dir_all(artifact_path.parent().unwrap()).unwrap();
        std::fs::write(&artifact_path, b"actual").unwrap();
        let lock_entry = LockEntry {
            name: "aws".into(),
            source: TEST_SOURCE.into(),
            kind: crate::provider_resolver::LockEntryKind::Revision {
                revision: "main".into(),
                resolved_sha: TEST_SHA.into(),
                artifact_id: 42,
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
        let wasm_path = cache_path_revision(dir.path(), &source, TEST_SHA);
        std::fs::create_dir_all(wasm_path.parent().unwrap()).unwrap();
        std::fs::write(&wasm_path, b"tampered provider").unwrap();
        let mut lock_file = crate::provider_resolver::LockFile::default();
        lock_file.upsert(crate::provider_resolver::LockEntry {
            name: "aws".into(),
            source: TEST_SOURCE.into(),
            kind: crate::provider_resolver::LockEntryKind::Revision {
                revision: "main".into(),
                resolved_sha: TEST_SHA.into(),
                artifact_id: 42,
            },
            sha256: "expected-sha256".into(),
            registry: None,
        });

        let install_lock = ProviderInstallLock::acquire(dir.path()).unwrap();
        let mut lock_file = install_lock.guard_lock_file_for_test(lock_file);
        let result = resolve_provider_by_revision(&mut lock_file, &source, "main", "aws", false);
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
    }

    #[test]
    fn fresh_revision_download_mismatch_is_rejected_before_publish() {
        let project = tempfile::tempdir().unwrap();
        let source = test_source();
        let destination = WasmArtifactPath::new(
            cache_path_revision(project.path(), &source, TEST_SHA),
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
                resolved_sha: TEST_SHA.into(),
                artifact_id: 42,
            },
            sha256: "locked-revision-sha256".into(),
            registry: None,
        });
        let pin = lookup_revision_artifact_pin(
            &lock_file,
            &source.source_key(),
            "main",
            TEST_SHA,
            "aws",
            true,
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
                resolved_sha: TEST_SHA.into(),
                artifact_id: 42,
            },
            sha256: expected_sha256.clone(),
            registry: None,
        });
        let lock_path = crate::provider_resolver::provider_lock_path(project.path());
        lock_file.save(&lock_path).unwrap();
        let lock_before = std::fs::read(&lock_path).unwrap();
        let global_wasm = global_cache_path_revision(&source, TEST_SHA).unwrap();
        std::fs::create_dir_all(global_wasm.as_path().parent().unwrap()).unwrap();
        std::fs::write(global_wasm.as_path(), cached_bytes).unwrap();
        let project_wasm = cache_path_revision(project.path(), &source, TEST_SHA);
        let actual_sha256 = sha256_file(global_wasm.as_path()).unwrap();
        let install_lock = ProviderInstallLock::acquire(project.path()).unwrap();
        let mut guarded = install_lock.guard_lock_file_for_test(lock_file);

        let result = resolve_provider_by_revision(&mut guarded, &source, "main", "aws", false);

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
        assert_eq!(std::fs::read(&lock_path).unwrap(), lock_before);
        assert_eq!(
            guarded
                .find_by_source_key_and_sha(&source.source_key(), TEST_SHA)
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
        let global_wasm = global_cache_path_revision(&source, TEST_SHA).unwrap();
        std::fs::create_dir_all(global_wasm.as_path().parent().unwrap()).unwrap();
        std::fs::write(global_wasm.as_path(), cached_bytes).unwrap();
        let sha256 = sha256_file(global_wasm.as_path()).unwrap();
        let mut lock_file = crate::provider_resolver::LockFile::default();
        lock_file.upsert(crate::provider_resolver::LockEntry {
            name: "aws".into(),
            source: TEST_SOURCE.into(),
            kind: crate::provider_resolver::LockEntryKind::Revision {
                revision: "main".into(),
                resolved_sha: TEST_SHA.into(),
                artifact_id: 42,
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
            resolve_provider_by_revision(&mut guarded, &source, "main", "aws", false).unwrap();

        assert_eq!(resolved_sha, TEST_SHA);
        assert_eq!(std::fs::read(resolved.as_path()).unwrap(), cached_bytes);
        assert_eq!(std::fs::read(&lock_path).unwrap(), lock_before);
        assert_eq!(
            guarded
                .find_by_source_key_and_sha(&source.source_key(), TEST_SHA)
                .unwrap()
                .sha256,
            sha256
        );
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
    const TEST_SHA: &str = "abc123def456";

    fn test_source() -> GithubSource {
        github_source("carina-rs", "carina-provider-aws")
    }

    fn artifact(name: &str, id: u64, expired: bool) -> Artifact {
        Artifact {
            id,
            name: name.to_string(),
            expired,
        }
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

    fn sha256_bytes(bytes: &[u8]) -> String {
        format!("{:x}", Sha256::digest(bytes))
    }

    #[test]
    fn revision_artifact_selection_uses_zip_contents_not_artifact_name() {
        let wasm = b"provider wasm";
        let selected = select_artifact_from_runs(
            &[100],
            &test_source(),
            TEST_SHA,
            |_| Ok(vec![artifact("build-output", 42, false)]),
            |artifact_id| {
                assert_eq!(artifact_id, 42);
                Ok(artifact_zip(&[("dist/provider.wasm", wasm)]))
            },
        )
        .unwrap();

        assert_eq!(selected.artifact_id(), 42);
        assert_eq!(selected.wasm_bytes(), wasm);
        assert_eq!(selected.sha256(), sha256_bytes(wasm));
    }

    #[test]
    fn revision_artifact_selection_scans_all_runs_and_artifacts() {
        let selected = select_artifact_from_runs(
            &[200, 100],
            &test_source(),
            TEST_SHA,
            |run_id| match run_id {
                200 => Ok(vec![artifact("docs", 1, false)]),
                100 => Ok(vec![
                    artifact("metadata", 2, false),
                    artifact("portable-build", 3, false),
                ]),
                other => panic!("unexpected run {other}"),
            },
            |artifact_id| match artifact_id {
                1 => Ok(artifact_zip(&[("README.txt", b"docs")])),
                2 => Ok(artifact_zip(&[("metadata.json", b"{}")])),
                3 => Ok(artifact_zip(&[("out/provider.wasm", b"selected")])),
                other => panic!("unexpected artifact {other}"),
            },
        )
        .unwrap();

        assert_eq!(selected.artifact_id(), 3);
        assert_eq!(selected.wasm_bytes(), b"selected");
    }

    #[test]
    fn revision_artifact_selection_no_wasm_lists_all_artifact_names() {
        let error = select_artifact_from_runs(
            &[100],
            &test_source(),
            TEST_SHA,
            |_| {
                Ok(vec![
                    artifact("docs", 1, false),
                    artifact("metadata", 2, false),
                ])
            },
            |artifact_id| match artifact_id {
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
            &[100],
            &test_source(),
            TEST_SHA,
            |_| {
                Ok(vec![
                    artifact("linux-build", 1, false),
                    artifact("macos-build", 2, false),
                ])
            },
            |artifact_id| match artifact_id {
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
    fn revision_artifact_selection_multiple_wasm_in_one_artifact_lists_each_entry() {
        let error = select_artifact_from_runs(
            &[100],
            &test_source(),
            TEST_SHA,
            |_| Ok(vec![artifact("build-output", 1, false)]),
            |_| {
                Ok(artifact_zip(&[
                    ("dist/provider.wasm", b"one"),
                    ("fixtures/helper.wasm", b"two"),
                ]))
            },
        )
        .unwrap_err();

        assert!(
            error.contains("build-output: dist/provider.wasm"),
            "{error}"
        );
        assert!(
            error.contains("build-output: fixtures/helper.wasm"),
            "{error}"
        );
    }

    #[test]
    fn revision_artifact_selection_only_expired_artifacts_reports_expired() {
        let error = select_artifact_from_runs(
            &[100],
            &test_source(),
            TEST_SHA,
            |_| {
                Ok(vec![
                    artifact("first-build", 1, true),
                    artifact("second-build", 2, true),
                ])
            },
            |_| -> Result<Vec<u8>, String> { panic!("expired artifacts must not be downloaded") },
        )
        .unwrap_err();

        assert!(error.contains("expired"), "{error}");
        assert!(error.contains("first-build"), "{error}");
        assert!(error.contains("second-build"), "{error}");
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
                resolved_sha: TEST_SHA.into(),
                artifact_id: 42,
            },
            sha256: sha256_bytes(wasm),
            registry: None,
        });
        let pin = lookup_revision_artifact_pin(
            &lock_file,
            &test_source().source_key(),
            "main",
            TEST_SHA,
            "aws",
            true,
        );
        let listed_runs = Cell::new(false);

        let selected = resolve_revision_artifact(
            &pin,
            &test_source(),
            TEST_SHA,
            || {
                listed_runs.set(true);
                Ok(vec![100])
            },
            |_| panic!("locked replay must not fetch run artifacts"),
            |artifact_id| {
                assert_eq!(artifact_id, 42);
                Ok(artifact_zip(&[("provider.wasm", wasm)]))
            },
        )
        .unwrap();

        assert!(!listed_runs.get());
        assert_eq!(selected.artifact_id(), 42);
        assert_eq!(selected.wasm_bytes(), wasm);
    }

    #[test]
    fn revision_artifact_locked_replay_rejects_sha256_mismatch() {
        let mut lock_file = crate::provider_resolver::LockFile::default();
        lock_file.upsert(LockEntry {
            name: "aws".into(),
            source: TEST_SOURCE.into(),
            kind: crate::provider_resolver::LockEntryKind::Revision {
                revision: "main".into(),
                resolved_sha: TEST_SHA.into(),
                artifact_id: 42,
            },
            sha256: sha256_bytes(b"locked provider"),
            registry: None,
        });
        let pin = lookup_revision_artifact_pin(
            &lock_file,
            &test_source().source_key(),
            "main",
            TEST_SHA,
            "aws",
            true,
        );

        let error = resolve_revision_artifact(
            &pin,
            &test_source(),
            TEST_SHA,
            || -> Result<Vec<u64>, String> { panic!("locked replay must not list runs") },
            |_| panic!("locked replay must not fetch run artifacts"),
            |_| Ok(artifact_zip(&[("provider.wasm", b"changed provider")])),
        )
        .unwrap_err();

        assert!(error.contains("SHA256 mismatch"), "{error}");
        assert!(error.contains("artifact 42"), "{error}");
    }

    #[test]
    fn revision_artifact_upgrade_reselects_instead_of_replaying_pin() {
        let mut lock_file = crate::provider_resolver::LockFile::default();
        lock_file.upsert(LockEntry {
            name: "aws".into(),
            source: TEST_SOURCE.into(),
            kind: crate::provider_resolver::LockEntryKind::Revision {
                revision: "main".into(),
                resolved_sha: TEST_SHA.into(),
                artifact_id: 42,
            },
            sha256: sha256_bytes(b"old provider"),
            registry: None,
        });
        let pin = lookup_revision_artifact_pin(
            &lock_file,
            &test_source().source_key(),
            "main",
            TEST_SHA,
            "aws",
            false,
        );
        let listed_runs = Cell::new(false);

        let selected = resolve_revision_artifact(
            &pin,
            &test_source(),
            TEST_SHA,
            || {
                listed_runs.set(true);
                Ok(vec![100])
            },
            |_| Ok(vec![artifact("new-build", 99, false)]),
            |artifact_id| {
                assert_eq!(artifact_id, 99);
                Ok(artifact_zip(&[("provider.wasm", b"new provider")]))
            },
        )
        .unwrap();

        assert!(listed_runs.get());
        assert_eq!(selected.artifact_id(), 99);
        assert_eq!(selected.wasm_bytes(), b"new provider");
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
        let parsed: ArtifactsResponse = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.artifacts.len(), 1);
        assert_eq!(parsed.artifacts[0].id, 12345);
        assert_eq!(parsed.artifacts[0].name, "carina-provider-aws.wasm");
        assert!(!parsed.artifacts[0].expired);
    }
}
