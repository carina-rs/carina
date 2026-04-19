//! Resolve provider binaries from GitHub Actions CI artifacts by git revision.

use serde::Deserialize;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

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

/// Extract owner and repo from a source string like "github.com/owner/repo".
fn parse_source(source: &str) -> Result<(&str, &str), String> {
    let parts: Vec<&str> = source.split('/').collect();
    if parts.len() != 3 || parts[0] != "github.com" {
        return Err(format!(
            "Invalid source format: {source}. Expected: github.com/{{owner}}/{{repo}}"
        ));
    }
    Ok((parts[1], parts[2]))
}

/// Resolve a git ref (branch, tag, or SHA) to a full commit SHA.
pub fn resolve_ref_to_sha(source: &str, revision: &str, token: &str) -> Result<String, String> {
    let (owner, repo) = parse_source(source)?;
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
/// both triggered on push to main). The caller is responsible for picking the
/// run that actually contains the expected artifact — see
/// [`select_artifact_from_runs`].
fn find_workflow_run_ids(source: &str, sha: &str, token: &str) -> Result<Vec<u64>, String> {
    let (owner, repo) = parse_source(source)?;
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
fn fetch_run_artifacts(source: &str, run_id: u64, token: &str) -> Result<Vec<Artifact>, String> {
    let (owner, repo) = parse_source(source)?;
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

/// Result of looking for a specific artifact inside a single run's artifact list.
#[derive(Debug, PartialEq, Eq)]
enum ArtifactMatch {
    /// A non-expired artifact with the expected name was found.
    Found(u64),
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
    source: &str,
    sha: &str,
    mut fetch: F,
) -> Result<u64, String>
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
fn download_artifact(
    source: &str,
    artifact_id: u64,
    dest: &Path,
    token: &str,
) -> Result<(), String> {
    let (owner, repo) = parse_source(source)?;
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

    // Extract the .wasm file from the ZIP
    let cursor = io::Cursor::new(zip_data);
    let mut archive =
        zip::ZipArchive::new(cursor).map_err(|e| format!("Failed to open artifact ZIP: {e}"))?;

    let expected_wasm = format!(
        "{}.wasm",
        source.split('/').next_back().unwrap_or("provider")
    );

    for i in 0..archive.len() {
        let mut file = archive
            .by_index(i)
            .map_err(|e| format!("Failed to read ZIP entry: {e}"))?;

        let name = file.name().to_string();
        if name.ends_with(".wasm") {
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent)
                    .map_err(|e| format!("Failed to create cache dir: {e}"))?;
            }
            let mut out = fs::File::create(dest)
                .map_err(|e| format!("Failed to create {}: {e}", dest.display()))?;
            io::copy(&mut file, &mut out).map_err(|e| format!("Failed to write WASM file: {e}"))?;
            return Ok(());
        }
    }

    Err(format!(
        "No .wasm file found in artifact ZIP. Expected '{expected_wasm}'."
    ))
}

/// Cache path for revision-based providers.
/// Uses `rev-{sha_prefix}` as the version directory.
pub fn cache_path_revision(base_dir: &Path, source: &str, sha: &str) -> PathBuf {
    let repo = source.split('/').next_back().unwrap_or("provider");
    let sha_prefix = &sha[..sha.len().min(12)];
    base_dir
        .join(".carina")
        .join("providers")
        .join(source)
        .join(format!("rev-{sha_prefix}"))
        .join(format!("{repo}.wasm"))
}

pub(crate) fn global_cache_path_revision(source: &str, sha: &str) -> Option<PathBuf> {
    let repo = source.split('/').next_back().unwrap_or("provider");
    let sha_prefix = &sha[..sha.len().min(12)];
    super::provider_resolver::global_cache_dir().map(|dir| {
        dir.join(source)
            .join(format!("rev-{sha_prefix}"))
            .join(format!("{repo}.wasm"))
    })
}

/// Resolve a provider by revision: download from CI artifacts if not cached.
pub fn resolve_provider_by_revision(
    base_dir: &Path,
    source: &str,
    revision: &str,
    name: &str,
    lock_file: &mut super::provider_resolver::LockFile,
    upgrade: bool,
) -> Result<(PathBuf, String), String> {
    let token = get_github_token()?;

    // 1. Resolve revision to SHA
    let sha = if !upgrade
        && let Some(lock_entry) = lock_file.find_by_source(source)
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
        resolve_ref_to_sha(source, revision, &token)?
    };

    // 2. Check cache
    let wasm_path = cache_path_revision(base_dir, source, &sha);
    if wasm_path.exists() {
        let actual_hash = super::provider_resolver::sha256_file(&wasm_path)
            .map_err(|e| format!("Failed to hash WASM binary: {e}"))?;
        if let Some(lock_entry) = lock_file.find_by_source_and_sha(source, &sha)
            && actual_hash != lock_entry.sha256
        {
            return Err(format!(
                "SHA256 mismatch for provider '{}' ({}@{}). Expected: {}, got: {}. Re-run `carina init` to re-download.",
                name,
                source,
                &sha[..12],
                lock_entry.sha256,
                actual_hash
            ));
        }
        // Record the entry so a caller that subsequently writes the lock
        // doesn't stomp it with an empty in-memory LockFile (issue #2032).
        lock_file.upsert(super::provider_resolver::LockEntry {
            name: name.to_string(),
            source: source.to_string(),
            kind: super::provider_resolver::LockEntryKind::Revision {
                revision: revision.to_string(),
                resolved_sha: sha.clone(),
            },
            sha256: actual_hash,
        });
        return Ok((wasm_path, sha));
    }

    // 2b. Check global plugin cache
    if let Some(global_wasm) = global_cache_path_revision(source, &sha)
        && global_wasm.exists()
    {
        if let Some(parent) = wasm_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        std::fs::hard_link(&global_wasm, &wasm_path)
            .or_else(|_| std::fs::copy(&global_wasm, &wasm_path).map(|_| ()))
            .map_err(|e| format!("Failed to link/copy from global cache: {e}"))?;
        let hash = super::provider_resolver::sha256_file(&wasm_path)
            .map_err(|e| format!("Failed to hash WASM binary: {e}"))?;
        lock_file.upsert(super::provider_resolver::LockEntry {
            name: name.to_string(),
            source: source.to_string(),
            kind: super::provider_resolver::LockEntryKind::Revision {
                revision: revision.to_string(),
                resolved_sha: sha.clone(),
            },
            sha256: hash,
        });
        eprintln!(
            "Installed provider '{}' from global cache ({}@{})",
            name,
            source,
            &sha[..sha.len().min(12)]
        );
        return Ok((wasm_path, sha));
    }

    // 3. Download from CI artifacts
    eprintln!(
        "Downloading provider '{}' from CI artifacts ({source}@{})...",
        name,
        &sha[..sha.len().min(12)]
    );

    let run_ids = find_workflow_run_ids(source, &sha, &token)?;
    let expected_name = format!(
        "{}.wasm",
        source.split('/').next_back().unwrap_or("provider")
    );
    let artifact_id =
        select_artifact_from_runs(&run_ids, &expected_name, source, &sha, |run_id| {
            fetch_run_artifacts(source, run_id, &token)
        })?;
    download_artifact(source, artifact_id, &wasm_path, &token)?;

    // Save to global cache
    if let Some(global_wasm) = global_cache_path_revision(source, &sha) {
        if let Some(parent) = global_wasm.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::hard_link(&wasm_path, &global_wasm)
            .or_else(|_| std::fs::copy(&wasm_path, &global_wasm).map(|_| ()));
    }

    let hash = super::provider_resolver::sha256_file(&wasm_path)
        .map_err(|e| format!("Failed to hash WASM binary: {e}"))?;

    lock_file.upsert(super::provider_resolver::LockEntry {
        name: name.to_string(),
        source: source.to_string(),
        kind: super::provider_resolver::LockEntryKind::Revision {
            revision: revision.to_string(),
            resolved_sha: sha.clone(),
        },
        sha256: hash,
    });

    eprintln!(
        "Installed provider '{}' ({source}@{})",
        name,
        &sha[..sha.len().min(12)]
    );

    Ok((wasm_path, sha))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_source_valid() {
        let (owner, repo) = parse_source("github.com/carina-rs/carina-provider-awscc").unwrap();
        assert_eq!(owner, "carina-rs");
        assert_eq!(repo, "carina-provider-awscc");
    }

    #[test]
    fn test_parse_source_invalid() {
        assert!(parse_source("invalid").is_err());
        assert!(parse_source("gitlab.com/foo/bar").is_err());
    }

    #[test]
    fn test_cache_path_revision() {
        let base = Path::new("/tmp/project");
        let sha = "abc123def456789012345678901234567890abcd";
        let path = cache_path_revision(base, "github.com/carina-rs/carina-provider-awscc", sha);
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
        let path = cache_path_revision(base, "github.com/carina-rs/carina-provider-awscc", sha);
        assert_eq!(
            path,
            PathBuf::from(
                "/tmp/project/.carina/providers/github.com/carina-rs/carina-provider-awscc/rev-abc123/carina-provider-awscc.wasm"
            )
        );
    }

    #[test]
    fn test_get_github_token_from_env() {
        // This test only verifies the env var path; gh CLI path is harder to unit test
        unsafe { std::env::set_var("GITHUB_TOKEN", "test-token-123") };
        let token = get_github_token().unwrap();
        assert_eq!(token, "test-token-123");
        unsafe { std::env::remove_var("GITHUB_TOKEN") };
    }

    const TEST_SOURCE: &str = "github.com/carina-rs/carina-provider-aws";
    const TEST_SHA: &str = "abc123def456";

    fn artifact(name: &str, id: u64, expired: bool) -> Artifact {
        Artifact {
            id,
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
            ArtifactMatch::Found(42)
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
            ArtifactMatch::Found(2)
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
            TEST_SOURCE,
            TEST_SHA,
            fetch,
        );
        assert_eq!(result.unwrap(), 999);
    }

    #[test]
    fn test_select_artifact_found_in_first_run() {
        let run_ids = vec![100u64];
        let fetch = |_| Ok(vec![artifact("my.wasm", 7, false)]);
        assert_eq!(
            select_artifact_from_runs(&run_ids, "my.wasm", TEST_SOURCE, TEST_SHA, fetch).unwrap(),
            7
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
            select_artifact_from_runs(&run_ids, "my.wasm", TEST_SOURCE, TEST_SHA, fetch).unwrap(),
            2
        );
    }

    #[test]
    fn test_select_artifact_not_found_anywhere() {
        let run_ids = vec![200u64, 100u64];
        let fetch = |_| Ok(vec![artifact("other.wasm", 1, false)]);
        let err = select_artifact_from_runs(&run_ids, "my.wasm", TEST_SOURCE, TEST_SHA, fetch)
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
        let err = select_artifact_from_runs(&run_ids, "my.wasm", TEST_SOURCE, TEST_SHA, fetch)
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
        let err = select_artifact_from_runs(&run_ids, "my.wasm", TEST_SOURCE, TEST_SHA, fetch)
            .unwrap_err();
        assert!(err.contains("network error for run 200"), "err={err}");
    }

    #[test]
    fn test_select_artifact_no_runs() {
        let run_ids: Vec<u64> = vec![];
        let fetch = |_| -> Result<Vec<Artifact>, String> {
            panic!("fetch should not be called when there are no runs")
        };
        let err = select_artifact_from_runs(&run_ids, "my.wasm", TEST_SOURCE, TEST_SHA, fetch)
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
        let parsed: ArtifactsResponse = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.artifacts.len(), 1);
        assert_eq!(parsed.artifacts[0].id, 12345);
        assert_eq!(parsed.artifacts[0].name, "carina-provider-aws.wasm");
        assert!(!parsed.artifacts[0].expired);
    }
}
