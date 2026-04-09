//! Resolve provider binaries from GitHub Actions CI artifacts by git revision.

use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

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

/// Find the most recent successful workflow run for a given commit SHA.
/// Returns the run ID.
fn find_workflow_run(source: &str, sha: &str, token: &str) -> Result<u64, String> {
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
    let data: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| format!("Failed to parse response: {e}"))?;

    let runs = data
        .get("workflow_runs")
        .and_then(|r| r.as_array())
        .ok_or_else(|| format!("No workflow_runs in response for {source}@{sha}"))?;

    if runs.is_empty() {
        return Err(format!(
            "No successful workflow runs found for {source}@{sha}. \
             Ensure CI has run successfully for this revision."
        ));
    }

    // Return the most recent run
    runs[0]
        .get("id")
        .and_then(|id| id.as_u64())
        .ok_or_else(|| "Failed to extract run ID".to_string())
}

/// Find the artifact ID for a WASM provider binary in a workflow run.
/// Looks for an artifact named `{repo}.wasm`.
fn find_wasm_artifact(source: &str, run_id: u64, token: &str) -> Result<u64, String> {
    let (owner, repo) = parse_source(source)?;
    let expected_name = format!("{repo}.wasm");
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
    let data: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| format!("Failed to parse response: {e}"))?;

    let artifacts = data
        .get("artifacts")
        .and_then(|a| a.as_array())
        .ok_or_else(|| "No artifacts in response".to_string())?;

    for artifact in artifacts {
        let name = artifact.get("name").and_then(|n| n.as_str()).unwrap_or("");
        if name == expected_name {
            if let Some(true) = artifact.get("expired").and_then(|e| e.as_bool()) {
                return Err(format!(
                    "Artifact '{expected_name}' has expired. \
                     Re-run CI for this revision or use `file://` for local builds."
                ));
            }
            return artifact
                .get("id")
                .and_then(|id| id.as_u64())
                .ok_or_else(|| "Failed to extract artifact ID".to_string());
        }
    }

    Err(format!(
        "No artifact named '{expected_name}' found in workflow run {run_id}. \
         Ensure CI uploads an artifact named '{expected_name}'."
    ))
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
        && lock_entry.revision.as_deref() == Some(revision)
        && lock_entry.resolved_sha.is_some()
    {
        // Reuse locked SHA if revision matches (skip on upgrade)
        lock_entry.resolved_sha.clone().unwrap()
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
        if let Some(lock_entry) = lock_file.find_by_source_and_sha(source, &sha) {
            let actual_hash = super::provider_resolver::sha256_file(&wasm_path)
                .map_err(|e| format!("Failed to hash WASM binary: {e}"))?;
            if actual_hash != lock_entry.sha256 {
                return Err(format!(
                    "SHA256 mismatch for provider '{}' ({}@{}). Expected: {}, got: {}. Re-run `carina init` to re-download.",
                    name,
                    source,
                    &sha[..12],
                    lock_entry.sha256,
                    actual_hash
                ));
            }
        }
        return Ok((wasm_path, sha));
    }

    // 3. Download from CI artifacts
    eprintln!(
        "Downloading provider '{}' from CI artifacts ({source}@{})...",
        name,
        &sha[..sha.len().min(12)]
    );

    let run_id = find_workflow_run(source, &sha, &token)?;
    let artifact_id = find_wasm_artifact(source, run_id, &token)?;
    download_artifact(source, artifact_id, &wasm_path, &token)?;

    let hash = super::provider_resolver::sha256_file(&wasm_path)
        .map_err(|e| format!("Failed to hash WASM binary: {e}"))?;

    lock_file.upsert(super::provider_resolver::LockEntry {
        name: name.to_string(),
        source: source.to_string(),
        version: String::new(),
        constraint: None,
        revision: Some(revision.to_string()),
        resolved_sha: Some(sha.clone()),
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
}
