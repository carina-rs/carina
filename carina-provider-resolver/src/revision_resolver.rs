//! Resolve provider binaries from GitHub Actions CI artifacts by git revision.

use std::io;
use std::path::{Path, PathBuf};

use carina_core::provider_source::GithubSource;

use crate::github::{
    FetchedLocation, GithubApi, GithubArtifactResponse, GithubHttp, UreqGithubHttp,
};
use crate::github_location::{ArtifactId, CommitSha, GitRef, RepoSlug};
use crate::install_lock::{
    GlobalArtifactPath, GuardedLockFile, ProviderInstallLock, StagedArtifact,
    global_cache_artifact_path, install_global_artifact_atomically,
    install_project_artifact_from_global_cache,
};
use crate::provider_resolver::WasmArtifactPath;
use crate::verified_artifact::{
    DownloadedArtifact, GithubRevisionDownloadLocation, RevisionArtifactPin, VerifiedArtifact,
    VerifiedArtifactError, lookup_revision_artifact_pin, publish_download,
};

/// GitHub Actions artifact metadata (subset used by the resolver).
struct Artifact {
    id: ArtifactId,
    name: String,
    expired: bool,
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
) -> Result<String, String> {
    let http = UreqGithubHttp;
    let api = GithubApi::new(&http, Some(token));
    resolve_ref_to_sha_with_api(source, revision, &api)
}

fn resolve_ref_to_sha_with_api<H: GithubHttp + ?Sized>(
    source: &GithubSource,
    revision: &str,
    api: &GithubApi<'_, H>,
) -> Result<String, String> {
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
) -> Result<String, String> {
    let commit = api.commit(repo, git_ref).map_err(|error| {
        format!("Failed to resolve revision '{revision}' for {source}: {error}")
    })?;
    Ok(commit.to_string())
}

/// Find all successful workflow runs for a given commit SHA.
/// Returns run IDs newest-first, matching GitHub's default ordering.
///
/// Multiple workflows may run on the same commit (e.g. `ci.yml` and `docs.yml`
/// both triggered on push to main). The caller is responsible for picking the
/// run that actually contains the expected artifact — see
/// [`select_artifact_from_runs`].
fn find_workflow_run_ids<H: GithubHttp + ?Sized>(
    source: &GithubSource,
    repo: &RepoSlug,
    sha: &str,
    api: &GithubApi<'_, H>,
) -> Result<Vec<u64>, String> {
    let sha = CommitSha::new(sha.to_string()).map_err(|error| error.to_string())?;
    api.workflow_runs(repo, &sha)
        .map_err(|error| format!("Failed to fetch workflow runs for {source}@{sha}: {error}"))
}

/// Fetch the artifacts list for a single workflow run.
fn fetch_run_artifacts<H: GithubHttp + ?Sized>(
    repo: &RepoSlug,
    run_id: u64,
    api: &GithubApi<'_, H>,
) -> Result<Vec<Artifact>, String> {
    api.run_artifacts(repo, run_id)
        .map(|artifacts| {
            artifacts
                .into_iter()
                .map(|GithubArtifactResponse { id, name, expired }| Artifact { id, name, expired })
                .collect()
        })
        .map_err(|error| format!("Failed to fetch artifacts for run {run_id}: {error}"))
}

/// Result of looking for a specific artifact inside a single run's artifact list.
#[derive(Debug, PartialEq, Eq)]
enum ArtifactMatch {
    /// A non-expired artifact with the expected name was found.
    Found(ArtifactId),
    /// The expected artifact exists in this run but has expired.
    Expired,
    /// No artifact with the expected name is in this run.
    NotFound,
}

/// Pure: scan a run's artifacts list for one matching `expected_name`.
/// Expired artifacts are reported separately so callers can surface a
/// targeted error message when every candidate run is expired.
fn pick_artifact_id(artifacts: &[Artifact], expected_name: &str) -> ArtifactMatch {
    let mut saw_expired = false;
    for artifact in artifacts {
        if artifact.name != expected_name {
            continue;
        }
        if artifact.expired {
            saw_expired = true;
            continue;
        }
        return ArtifactMatch::Found(artifact.id);
    }
    if saw_expired {
        ArtifactMatch::Expired
    } else {
        ArtifactMatch::NotFound
    }
}

/// Iterate candidate workflow runs and return the first artifact ID that
/// matches `expected_name` and is not expired.
///
/// When a repo has multiple workflows on push (e.g. `ci.yml` producing a WASM
/// artifact and `docs.yml` producing nothing), GitHub returns all successful
/// runs for the same SHA. Taking only `runs[0]` picks whichever finished last,
/// which may not be the run that uploaded the artifact. Iterating the full
/// list removes the dependency on workflow ordering.
fn select_artifact_from_runs<F>(
    run_ids: &[u64],
    expected_name: &str,
    source: &GithubSource,
    sha: &str,
    mut fetch: F,
) -> Result<ArtifactId, String>
where
    F: FnMut(u64) -> Result<Vec<Artifact>, String>,
{
    if run_ids.is_empty() {
        return Err(format!(
            "No successful workflow runs found for {source}@{sha}. \
             Ensure CI has run successfully. Expected artifact: '{expected_name}'."
        ));
    }

    let mut saw_expired = false;
    for &run_id in run_ids {
        let artifacts = fetch(run_id)?;
        match pick_artifact_id(&artifacts, expected_name) {
            ArtifactMatch::Found(id) => return Ok(id),
            ArtifactMatch::Expired => saw_expired = true,
            ArtifactMatch::NotFound => {}
        }
    }

    if saw_expired {
        Err(format!(
            "Artifact '{expected_name}' has expired in every successful workflow run for {source}@{sha}. \
             Re-run CI for this revision or use `file://` for local builds."
        ))
    } else {
        Err(format!(
            "No artifact named '{expected_name}' found in any successful workflow run for {source}@{sha}. \
             Ensure CI uploads an artifact named '{expected_name}'."
        ))
    }
}

/// Download a GitHub Actions artifact (ZIP) and extract the WASM file.
fn download_artifact<'lock, H: GithubHttp + ?Sized>(
    install_lock: &'lock ProviderInstallLock,
    source: &GithubSource,
    repo: &RepoSlug,
    artifact_id: ArtifactId,
    destination: &WasmArtifactPath,
    api: &GithubApi<'_, H>,
) -> Result<
    (
        StagedArtifact<'lock>,
        FetchedLocation<GithubRevisionDownloadLocation>,
    ),
    String,
> {
    let (zip_data, download_location) = api
        .download_artifact_zip(repo, artifact_id)
        .map_err(|error| format!("Failed to download artifact {artifact_id}: {error}"))?
        .into_parts();

    // Extract the .wasm file from the ZIP
    let cursor = io::Cursor::new(zip_data);
    let mut archive =
        zip::ZipArchive::new(cursor).map_err(|e| format!("Failed to open artifact ZIP: {e}"))?;

    let expected_wasm = format!("{}.wasm", source.repo());

    for i in 0..archive.len() {
        let mut file = archive
            .by_index(i)
            .map_err(|e| format!("Failed to read ZIP entry: {e}"))?;

        let name = file.name().to_string();
        if name.ends_with(".wasm") {
            let mut staged =
                StagedArtifact::for_project(install_lock, destination).map_err(|e| {
                    format!(
                        "Failed to stage provider artifact {}: {e}",
                        destination.as_path().display()
                    )
                })?;
            io::copy(&mut file, &mut staged)
                .map_err(|e| format!("Failed to write WASM file: {e}"))?;
            return Ok((staged, download_location));
        }
    }

    Err(format!(
        "No .wasm file found in artifact ZIP. Expected '{expected_wasm}'."
    ))
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
    let token = get_github_token()?;
    let http = UreqGithubHttp;
    let api = GithubApi::new(&http, Some(&token));
    resolve_provider_by_revision_with_api(lock_file, source, revision, name, upgrade, &api)
}

pub(crate) fn resolve_provider_by_revision_with_api<H: GithubHttp + ?Sized>(
    lock_file: &mut GuardedLockFile<'_>,
    source: &GithubSource,
    revision: &str,
    name: &str,
    upgrade: bool,
    api: &GithubApi<'_, H>,
) -> Result<(WasmArtifactPath, String), String> {
    let (repo, git_ref) = validate_revision_location(source, revision)?;
    let install_lock = lock_file.install_lock();
    let base_dir = install_lock.project_dir();
    let source_key = source.source_key();

    // 1. Resolve revision to SHA
    let sha = if !upgrade
        && let Some(lock_entry) = lock_file.find_by_source_key(&source_key)
        && let super::provider_resolver::LockEntryKind::Revision {
            revision: locked_revision,
            resolved_sha,
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
        resolve_typed_ref_to_sha(source, revision, &repo, &git_ref, api)?
    };

    let artifact_pin = lookup_revision_artifact_pin(lock_file, &source_key, revision, &sha, name);

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

    let run_ids = find_workflow_run_ids(source, &repo, &sha, api)?;
    let expected_name = format!("{}.wasm", source.repo());
    let artifact_id =
        select_artifact_from_runs(&run_ids, &expected_name, source, &sha, |run_id| {
            fetch_run_artifacts(&repo, run_id, api)
        })?;
    let (staged, download_location) =
        download_artifact(install_lock, source, &repo, artifact_id, &wasm_path, api)?;
    let installed_path = match publish_download(staged, artifact_pin, download_location)? {
        DownloadedArtifact::Verified { artifact, pin } => {
            record_verified_revision_pin(lock_file, &artifact, pin, name);
            artifact.path().clone()
        }
        DownloadedArtifact::FirstUse { path, sha256 } => {
            lock_file.upsert(super::provider_resolver::LockEntry {
                name: name.to_string(),
                source: source_key.to_string(),
                kind: super::provider_resolver::LockEntryKind::Revision {
                    revision: revision.to_string(),
                    resolved_sha: sha.clone(),
                },
                sha256,
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
    use crate::github::FixtureGithubHttp;
    use crate::provider_resolver::{LockEntry, RegistryLock, sha256_file};
    use carina_core::provider_source::{ProviderSource, RemoteSource};
    use std::io::Write as _;

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
        let trust = tempfile::tempdir().unwrap();
        let _trust_env = crate::ScopedEnvVar::set("CARINA_TRUST_RECORD_DIR", trust.path());
        let source = test_source();
        let expected_sha256 = "0".repeat(64);
        crate::verified_artifact::record_test_github_origin_download(
            &source.source_key(),
            &expected_sha256,
            None,
        )
        .unwrap();
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
            },
            sha256: expected_sha256,
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
            },
            sha256: "locked-revision-sha256".into(),
            registry: None,
        });
        let pin =
            lookup_revision_artifact_pin(&lock_file, &source.source_key(), "main", TEST_SHA, "aws");
        let location = FetchedLocation::for_test(GithubRevisionDownloadLocation::new(
            RepoSlug::try_from(&source).unwrap(),
            ArtifactId::from_lock(42),
        ));

        let error = publish_download(staged, pin, location)
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
        let trust = tempfile::tempdir().unwrap();
        let _token = crate::ScopedEnvVar::set("GITHUB_TOKEN", "test-token");
        let _cache = crate::ScopedEnvVar::set("CARINA_PLUGIN_CACHE_DIR", global.path());
        let _trust_env = crate::ScopedEnvVar::set("CARINA_TRUST_RECORD_DIR", trust.path());
        let source = test_source();
        let locked_bytes = b"locked revision provider";
        let cached_bytes = b"tampered global revision provider";
        let expected_file = project.path().join("expected.wasm");
        std::fs::write(&expected_file, locked_bytes).unwrap();
        let expected_sha256 = sha256_file(&expected_file).unwrap();
        crate::verified_artifact::record_test_github_origin_download(
            &source.source_key(),
            &expected_sha256,
            None,
        )
        .unwrap();
        let mut lock_file = crate::provider_resolver::LockFile::default();
        lock_file.upsert(crate::provider_resolver::LockEntry {
            name: "aws".into(),
            source: TEST_SOURCE.into(),
            kind: crate::provider_resolver::LockEntryKind::Revision {
                revision: "main".into(),
                resolved_sha: TEST_SHA.into(),
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
        let trust = tempfile::tempdir().unwrap();
        let _token = crate::ScopedEnvVar::set("GITHUB_TOKEN", "test-token");
        let _cache = crate::ScopedEnvVar::set("CARINA_PLUGIN_CACHE_DIR", global.path());
        let _trust_env = crate::ScopedEnvVar::set("CARINA_TRUST_RECORD_DIR", trust.path());
        let source = test_source();
        let cached_bytes = b"lock-matching global revision provider";
        let global_wasm = global_cache_path_revision(&source, TEST_SHA).unwrap();
        std::fs::create_dir_all(global_wasm.as_path().parent().unwrap()).unwrap();
        std::fs::write(global_wasm.as_path(), cached_bytes).unwrap();
        let sha256 = sha256_file(global_wasm.as_path()).unwrap();
        crate::verified_artifact::record_test_github_origin_download(
            &source.source_key(),
            &sha256,
            None,
        )
        .unwrap();
        let mut lock_file = crate::provider_resolver::LockFile::default();
        lock_file.upsert(crate::provider_resolver::LockEntry {
            name: "aws".into(),
            source: TEST_SOURCE.into(),
            kind: crate::provider_resolver::LockEntryKind::Revision {
                revision: "main".into(),
                resolved_sha: TEST_SHA.into(),
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
    const TEST_SHA: &str = "0123456789abcdef0123456789abcdef01234567";

    fn test_source() -> GithubSource {
        github_source("carina-rs", "carina-provider-aws")
    }

    fn artifact(name: &str, id: u64, expired: bool) -> Artifact {
        Artifact {
            id: ArtifactId::from_lock(id),
            name: name.to_string(),
            expired,
        }
    }

    #[test]
    fn test_pick_artifact_id_found() {
        let artifacts = vec![
            artifact("other.wasm", 1, false),
            artifact("carina-provider-aws.wasm", 42, false),
        ];
        assert_eq!(
            pick_artifact_id(&artifacts, "carina-provider-aws.wasm"),
            ArtifactMatch::Found(ArtifactId::from_lock(42))
        );
    }

    #[test]
    fn test_pick_artifact_id_not_found() {
        let artifacts = vec![artifact("other.wasm", 1, false)];
        assert_eq!(
            pick_artifact_id(&artifacts, "carina-provider-aws.wasm"),
            ArtifactMatch::NotFound
        );
    }

    #[test]
    fn test_pick_artifact_id_expired() {
        let artifacts = vec![artifact("carina-provider-aws.wasm", 42, true)];
        assert_eq!(
            pick_artifact_id(&artifacts, "carina-provider-aws.wasm"),
            ArtifactMatch::Expired
        );
    }

    #[test]
    fn test_pick_artifact_id_prefers_non_expired_over_expired() {
        // Same name, expired first then valid — must not short-circuit on expired.
        let artifacts = vec![artifact("my.wasm", 1, true), artifact("my.wasm", 2, false)];
        assert_eq!(
            pick_artifact_id(&artifacts, "my.wasm"),
            ArtifactMatch::Found(ArtifactId::from_lock(2))
        );
    }

    #[test]
    fn test_select_artifact_iterates_past_run_without_match() {
        // Regression for the multi-workflow case:
        // Run 200 is a docs workflow (no artifacts), run 100 is CI (has artifact).
        // GitHub returns them newest-first, so 200 comes first. The resolver
        // must iterate past 200 and find the artifact in 100.
        let run_ids = vec![200u64, 100u64];
        let fetch = |run_id: u64| -> Result<Vec<Artifact>, String> {
            match run_id {
                200 => Ok(vec![]),
                100 => Ok(vec![artifact("carina-provider-aws.wasm", 999, false)]),
                other => panic!("unexpected run_id {other}"),
            }
        };

        let result = select_artifact_from_runs(
            &run_ids,
            "carina-provider-aws.wasm",
            &test_source(),
            TEST_SHA,
            fetch,
        );
        assert_eq!(result.unwrap(), ArtifactId::from_lock(999));
    }

    #[test]
    fn test_select_artifact_found_in_first_run() {
        let run_ids = vec![100u64];
        let fetch = |_| Ok(vec![artifact("my.wasm", 7, false)]);
        assert_eq!(
            select_artifact_from_runs(&run_ids, "my.wasm", &test_source(), TEST_SHA, fetch)
                .unwrap(),
            ArtifactId::from_lock(7)
        );
    }

    #[test]
    fn test_select_artifact_prefers_later_run_when_earlier_is_expired() {
        // First run's artifact has expired, second run has a valid one.
        // The `saw_expired` flag must not suppress a later `Found`.
        let run_ids = vec![200u64, 100u64];
        let fetch = |run_id: u64| -> Result<Vec<Artifact>, String> {
            match run_id {
                200 => Ok(vec![artifact("my.wasm", 1, true)]),
                100 => Ok(vec![artifact("my.wasm", 2, false)]),
                other => panic!("unexpected run_id {other}"),
            }
        };
        assert_eq!(
            select_artifact_from_runs(&run_ids, "my.wasm", &test_source(), TEST_SHA, fetch)
                .unwrap(),
            ArtifactId::from_lock(2)
        );
    }

    #[test]
    fn test_select_artifact_not_found_anywhere() {
        let run_ids = vec![200u64, 100u64];
        let fetch = |_| Ok(vec![artifact("other.wasm", 1, false)]);
        let err = select_artifact_from_runs(&run_ids, "my.wasm", &test_source(), TEST_SHA, fetch)
            .unwrap_err();
        assert!(err.contains("No artifact named 'my.wasm'"), "err={err}");
        assert!(
            err.contains(TEST_SOURCE),
            "err should include source: {err}"
        );
        assert!(err.contains(TEST_SHA), "err should include sha: {err}");
    }

    #[test]
    fn test_select_artifact_expired_in_all_runs() {
        let run_ids = vec![200u64, 100u64];
        let fetch = |_| Ok(vec![artifact("my.wasm", 1, true)]);
        let err = select_artifact_from_runs(&run_ids, "my.wasm", &test_source(), TEST_SHA, fetch)
            .unwrap_err();
        assert!(err.contains("expired"), "err={err}");
        assert!(
            err.contains(TEST_SOURCE),
            "err should include source: {err}"
        );
        assert!(err.contains(TEST_SHA), "err should include sha: {err}");
    }

    #[test]
    fn test_select_artifact_propagates_fetch_error() {
        // A transient fetch error on the first run must be surfaced, not
        // silently swallowed so that later runs are tried.
        let run_ids = vec![200u64, 100u64];
        let fetch = |run_id: u64| -> Result<Vec<Artifact>, String> {
            Err(format!("network error for run {run_id}"))
        };
        let err = select_artifact_from_runs(&run_ids, "my.wasm", &test_source(), TEST_SHA, fetch)
            .unwrap_err();
        assert!(err.contains("network error for run 200"), "err={err}");
    }

    #[test]
    fn test_select_artifact_no_runs() {
        let run_ids: Vec<u64> = vec![];
        let fetch = |_| -> Result<Vec<Artifact>, String> {
            panic!("fetch should not be called when there are no runs")
        };
        let err = select_artifact_from_runs(&run_ids, "my.wasm", &test_source(), TEST_SHA, fetch)
            .unwrap_err();
        assert!(err.contains("No successful workflow runs"), "err={err}");
        assert!(
            err.contains(TEST_SOURCE),
            "err should include source: {err}"
        );
        assert!(err.contains(TEST_SHA), "err should include sha: {err}");
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
        let url =
            "https://api.github.com/repos/carina-rs/carina-provider-aws/actions/runs/7/artifacts";
        let http = FixtureGithubHttp::default().json(url, json);
        let api = GithubApi::new(&http, Some("token"));
        let repo = RepoSlug::try_from(&test_source()).unwrap();
        let parsed = api.run_artifacts(&repo, 7).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].id.get(), 12345);
        assert_eq!(parsed[0].name, "carina-provider-aws.wasm");
        assert!(!parsed[0].expired);
    }
}
