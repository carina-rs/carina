//! Resolve version constraints against GitHub Releases API.

#![allow(dead_code)]

use semver::{Version, VersionReq};

/// A resolved version from a GitHub release.
#[derive(Debug, Clone)]
pub struct ResolvedVersion {
    pub version: Version,
    pub tag: String,
}

/// Parse a GitHub release tag into a semver Version.
/// Strips the leading `v` prefix if present.
fn parse_tag(tag: &str) -> Option<Version> {
    let stripped = tag.strip_prefix('v').unwrap_or(tag);
    Version::parse(stripped).ok()
}

/// Given a list of release tags, find the highest version matching the constraint.
pub fn resolve_from_tags(tags: &[String], constraint: &VersionReq) -> Option<ResolvedVersion> {
    let mut candidates: Vec<(Version, &String)> = tags
        .iter()
        .filter_map(|tag| parse_tag(tag).map(|v| (v, tag)))
        .filter(|(v, _)| constraint.matches(v))
        .collect();

    candidates.sort_by(|(a, _), (b, _)| b.cmp(a)); // highest first
    candidates
        .into_iter()
        .next()
        .map(|(version, tag)| ResolvedVersion {
            version,
            tag: tag.clone(),
        })
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

/// Fetch release tags from GitHub Releases API.
pub fn fetch_release_tags(source: &str) -> Result<Vec<String>, String> {
    let (owner, repo) = parse_source(source)?;
    let url = format!("https://api.github.com/repos/{owner}/{repo}/releases?per_page=100");
    let response = ureq::get(&url)
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", "carina")
        .call()
        .map_err(|e| format!("Failed to fetch releases from {url}: {e}"))?;

    if response.status() != 200 {
        return Err(format!(
            "GitHub API returned status {} for {url}",
            response.status()
        ));
    }

    let body: String = response
        .into_body()
        .read_to_string()
        .map_err(|e| format!("Failed to read response body: {e}"))?;
    let releases: Vec<serde_json::Value> =
        serde_json::from_str(&body).map_err(|e| format!("Failed to parse GitHub response: {e}"))?;

    Ok(releases
        .iter()
        .filter_map(|r| r.get("tag_name")?.as_str().map(|s| s.to_string()))
        .collect())
}

/// Fetch the latest release tag from GitHub.
pub fn fetch_latest_tag(source: &str) -> Result<String, String> {
    let (owner, repo) = parse_source(source)?;
    let url = format!("https://api.github.com/repos/{owner}/{repo}/releases/latest");
    let response = ureq::get(&url)
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", "carina")
        .call()
        .map_err(|e| format!("Failed to fetch latest release from {url}: {e}"))?;

    if response.status() != 200 {
        return Err(format!(
            "GitHub API returned status {} for {url}",
            response.status()
        ));
    }

    let body: String = response
        .into_body()
        .read_to_string()
        .map_err(|e| format!("Failed to read response body: {e}"))?;
    let release: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| format!("Failed to parse GitHub response: {e}"))?;

    release
        .get("tag_name")
        .and_then(|t| t.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| "No tag_name in latest release".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_tilde_constraint() {
        let tags = vec![
            "v0.4.0".to_string(),
            "v0.5.0".to_string(),
            "v0.5.1".to_string(),
            "v0.5.2".to_string(),
            "v0.6.0".to_string(),
        ];
        let req = VersionReq::parse("~0.5.0").unwrap();
        let resolved = resolve_from_tags(&tags, &req).unwrap();
        assert_eq!(resolved.version, Version::new(0, 5, 2));
        assert_eq!(resolved.tag, "v0.5.2");
    }

    #[test]
    fn resolve_caret_constraint() {
        let tags = vec![
            "v1.0.0".to_string(),
            "v1.2.0".to_string(),
            "v1.9.0".to_string(),
            "v2.0.0".to_string(),
        ];
        let req = VersionReq::parse("^1.2.0").unwrap();
        let resolved = resolve_from_tags(&tags, &req).unwrap();
        assert_eq!(resolved.version, Version::new(1, 9, 0));
    }

    #[test]
    fn resolve_no_match() {
        let tags = vec!["v0.1.0".to_string()];
        let req = VersionReq::parse("~0.5.0").unwrap();
        assert!(resolve_from_tags(&tags, &req).is_none());
    }

    #[test]
    fn resolve_tags_without_v_prefix() {
        let tags = vec!["0.5.0".to_string(), "0.5.1".to_string()];
        let req = VersionReq::parse("~0.5.0").unwrap();
        let resolved = resolve_from_tags(&tags, &req).unwrap();
        assert_eq!(resolved.version, Version::new(0, 5, 1));
    }

    #[test]
    fn resolve_star_picks_highest() {
        let tags = vec![
            "v0.1.0".to_string(),
            "v1.0.0".to_string(),
            "v2.0.0".to_string(),
        ];
        let req = VersionReq::parse("*").unwrap();
        let resolved = resolve_from_tags(&tags, &req).unwrap();
        assert_eq!(resolved.version, Version::new(2, 0, 0));
    }

    #[test]
    fn parse_source_valid() {
        let (owner, repo) = parse_source("github.com/carina-rs/carina-provider-aws").unwrap();
        assert_eq!(owner, "carina-rs");
        assert_eq!(repo, "carina-provider-aws");
    }

    #[test]
    fn parse_source_invalid() {
        assert!(parse_source("invalid").is_err());
        assert!(parse_source("gitlab.com/foo/bar").is_err());
    }
}
