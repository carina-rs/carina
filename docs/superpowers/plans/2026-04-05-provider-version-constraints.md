# Provider Version Constraints Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add semver version constraint support (`~0.5.0`, `^1.2.0`) to provider configuration, with GitHub Releases-based version resolution and lock file integrity checking.

**Architecture:** The `version` field in `ProviderConfig` changes from `Option<String>` to `Option<VersionConstraint>` wrapping `semver::VersionReq`. The CLI resolves constraints against GitHub Releases API, stores resolved versions in `carina.lock`, and validates at plan/apply time. The provider protocol adds a `version` field to `ProviderInfo` for load-time verification.

**Tech Stack:** `semver` crate for constraint parsing/matching, `ureq` (already in carina-cli) for GitHub API calls, existing TOML lock file format.

**Spec:** `docs/superpowers/specs/2026-04-05-provider-version-constraints-design.md`

---

### Task 1: Add `VersionConstraint` type to `carina-core`

**Files:**
- Modify: `carina-core/Cargo.toml`
- Create: `carina-core/src/version_constraint.rs`
- Modify: `carina-core/src/lib.rs`

- [ ] **Step 1: Add `semver` dependency to `carina-core`**

In `carina-core/Cargo.toml`, add to `[dependencies]`:

```toml
semver = "1"
```

- [ ] **Step 2: Write the failing test for `VersionConstraint`**

Create `carina-core/src/version_constraint.rs`:

```rust
use semver::VersionReq;
use serde::{Deserialize, Serialize};
use std::fmt;

/// A parsed semver version constraint (e.g., "~0.5.0", "^1.2.0").
#[derive(Debug, Clone)]
pub struct VersionConstraint {
    /// Original constraint string from the DSL.
    pub raw: String,
    /// Parsed semver requirement.
    pub req: VersionReq,
}

impl VersionConstraint {
    pub fn parse(s: &str) -> Result<Self, String> {
        let req = VersionReq::parse(s)
            .map_err(|e| format!("Invalid version constraint '{}': {}", s, e))?;
        Ok(Self {
            raw: s.to_string(),
            req,
        })
    }

    /// Check if a version string satisfies this constraint.
    pub fn matches(&self, version: &str) -> Result<bool, String> {
        let ver = semver::Version::parse(version)
            .map_err(|e| format!("Invalid version '{}': {}", version, e))?;
        Ok(self.req.matches(&ver))
    }
}

impl fmt::Display for VersionConstraint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.raw)
    }
}

impl Serialize for VersionConstraint {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.raw)
    }
}

impl<'de> Deserialize<'de> for VersionConstraint {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        VersionConstraint::parse(&s).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_tilde_constraint() {
        let c = VersionConstraint::parse("~0.5.0").unwrap();
        assert_eq!(c.raw, "~0.5.0");
        assert!(c.matches("0.5.0").unwrap());
        assert!(c.matches("0.5.9").unwrap());
        assert!(!c.matches("0.6.0").unwrap());
    }

    #[test]
    fn parse_caret_constraint() {
        let c = VersionConstraint::parse("^1.2.0").unwrap();
        assert!(c.matches("1.2.0").unwrap());
        assert!(c.matches("1.9.0").unwrap());
        assert!(!c.matches("2.0.0").unwrap());
    }

    #[test]
    fn parse_exact_version() {
        let c = VersionConstraint::parse("=0.5.0").unwrap();
        assert!(c.matches("0.5.0").unwrap());
        assert!(!c.matches("0.5.1").unwrap());
    }

    #[test]
    fn parse_range_constraint() {
        let c = VersionConstraint::parse(">=0.5.0, <1.0.0").unwrap();
        assert!(c.matches("0.5.0").unwrap());
        assert!(c.matches("0.9.9").unwrap());
        assert!(!c.matches("1.0.0").unwrap());
    }

    #[test]
    fn parse_star_constraint() {
        let c = VersionConstraint::parse("*").unwrap();
        assert!(c.matches("0.1.0").unwrap());
        assert!(c.matches("99.0.0").unwrap());
    }

    #[test]
    fn parse_invalid_constraint() {
        assert!(VersionConstraint::parse("not-a-version").is_err());
    }

    #[test]
    fn display_shows_raw() {
        let c = VersionConstraint::parse("~0.5.0").unwrap();
        assert_eq!(format!("{c}"), "~0.5.0");
    }

    #[test]
    fn serde_roundtrip() {
        let c = VersionConstraint::parse("~0.5.0").unwrap();
        let json = serde_json::to_string(&c).unwrap();
        assert_eq!(json, "\"~0.5.0\"");
        let c2: VersionConstraint = serde_json::from_str(&json).unwrap();
        assert_eq!(c2.raw, "~0.5.0");
    }
}
```

- [ ] **Step 3: Register the module in `lib.rs`**

In `carina-core/src/lib.rs`, add:

```rust
pub mod version_constraint;
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p carina-core version_constraint`

Expected: All 8 tests pass.

- [ ] **Step 5: Commit**

```bash
git add carina-core/Cargo.toml carina-core/src/version_constraint.rs carina-core/src/lib.rs
git commit -m "feat: add VersionConstraint type with semver parsing"
```

---

### Task 2: Change `ProviderConfig.version` to `VersionConstraint`

**Files:**
- Modify: `carina-core/src/parser/mod.rs`

- [ ] **Step 1: Write the failing test**

Add this test to `carina-core/src/parser/mod.rs` in the `tests` module (near line 5889):

```rust
    #[test]
    fn parse_provider_block_with_version_constraint() {
        let input = r#"
            provider mock {
                source = "github.com/carina-rs/carina-provider-mock"
                version = "~0.5.0"
            }
        "#;
        let parsed = parse(input, &ProviderContext::default()).unwrap();
        let provider = &parsed.providers[0];
        let vc = provider.version.as_ref().unwrap();
        assert_eq!(vc.raw, "~0.5.0");
        assert!(vc.matches("0.5.3").unwrap());
        assert!(!vc.matches("0.6.0").unwrap());
    }

    #[test]
    fn parse_provider_block_with_invalid_version_constraint() {
        let input = r#"
            provider mock {
                source = "github.com/carina-rs/carina-provider-mock"
                version = "not-valid"
            }
        "#;
        let result = parse(input, &ProviderContext::default());
        assert!(result.is_err());
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p carina-core parse_provider_block_with_version_constraint`

Expected: FAIL — `version` is still `Option<String>`.

- [ ] **Step 3: Update `ProviderConfig` and version extraction**

In `carina-core/src/parser/mod.rs`, add the import at the top of the file (with other `use` statements):

```rust
use crate::version_constraint::VersionConstraint;
```

Change the `ProviderConfig` struct (around line 282):

```rust
    /// Provider version constraint (e.g., "~0.5.0", "^1.2.0").
    /// Extracted from the provider block and not passed to the provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<VersionConstraint>,
```

Change the version extraction code (around line 1879):

```rust
    // Extract version from attributes if present
    let version = if let Some(Value::String(v)) = attributes.remove("version") {
        Some(VersionConstraint::parse(&v).map_err(|e| {
            pest::error::Error::new_from_pos(
                pest::error::ErrorVariant::CustomError { message: e },
                pest::Position::from_start(""),
            )
        })?)
    } else {
        None
    };
```

- [ ] **Step 4: Update the existing `parse_provider_block_with_source_and_version` test**

The existing test at line 5889 uses `version.as_deref()` which no longer works. Update it:

```rust
    #[test]
    fn parse_provider_block_with_source_and_version() {
        let input = r#"
            provider mock {
                source = "github.com/carina-rs/carina-provider-mock"
                version = "0.1.0"
            }
        "#;
        let parsed = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(parsed.providers.len(), 1);

        let provider = &parsed.providers[0];
        assert_eq!(provider.name, "mock");
        assert_eq!(
            provider.source.as_deref(),
            Some("github.com/carina-rs/carina-provider-mock")
        );
        let vc = provider.version.as_ref().unwrap();
        assert_eq!(vc.raw, "0.1.0");
        // source and version should NOT be in attributes
        assert!(!provider.attributes.contains_key("source"));
        assert!(!provider.attributes.contains_key("version"));
    }
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p carina-core parse_provider_block_with`

Expected: All 4 provider parsing tests pass (existing + 2 new).

- [ ] **Step 6: Fix compilation across downstream crates**

Run: `cargo check`

The type change from `Option<String>` to `Option<VersionConstraint>` will break code in `carina-cli` that uses `.version.as_deref()`. Find and fix all call sites — they will be updated in later tasks, but the code must compile now. Temporarily adapt call sites to use `.version.as_ref().map(|v| v.raw.as_str())` where an exact version string is needed.

- [ ] **Step 7: Commit**

```bash
git add carina-core/src/parser/mod.rs
git commit -m "feat: change ProviderConfig.version to VersionConstraint"
```

---

### Task 3: Add `version` field to `ProviderInfo` in protocol

**Files:**
- Modify: `carina-provider-protocol/src/types.rs`
- Modify: `carina-plugin-host/src/wasm_convert.rs`
- Modify: `carina-plugin-host/src/wasm_factory.rs`

- [ ] **Step 1: Add `version` to `ProviderInfo`**

In `carina-provider-protocol/src/types.rs` (around line 128), add the `version` field:

```rust
/// Provider metadata returned by `provider_info`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderInfo {
    pub name: String,
    pub display_name: String,
    #[serde(default)]
    pub capabilities: Vec<String>,
    pub version: String,
}
```

- [ ] **Step 2: Update `json_to_provider_info` in `wasm_convert.rs`**

In `carina-plugin-host/src/wasm_convert.rs` (around line 219), update the function to return version:

```rust
/// Deserialize JSON to (name, display_name, version) tuple from ProviderInfo.
pub fn json_to_provider_info(json: &str) -> (String, String, String) {
    if let Ok(info) = serde_json::from_str::<proto::ProviderInfo>(json) {
        (info.name, info.display_name, info.version)
    } else {
        ("unknown".to_string(), "Unknown Provider".to_string(), "0.0.0".to_string())
    }
}
```

- [ ] **Step 3: Update call site in `wasm_factory.rs`**

In `carina-plugin-host/src/wasm_factory.rs` (around line 611), update the destructuring:

```rust
        let (name, display_name, version) = wasm_convert::json_to_provider_info(&info_json);
```

Add a `version` field to the `WasmProviderFactory` struct (around line 470):

```rust
    version: String,
```

Store it in the constructor where the struct is built (in the `Ok(Self {` block):

```rust
            version,
```

- [ ] **Step 4: Add a public accessor for the version**

Add a method to `WasmProviderFactory`:

```rust
    /// Returns the provider's reported version.
    pub fn version(&self) -> &str {
        &self.version
    }
```

- [ ] **Step 5: Fix compilation and run tests**

Run: `cargo check -p carina-plugin-host && cargo test -p carina-provider-protocol && cargo test -p carina-plugin-host`

Expected: All tests pass.

- [ ] **Step 6: Commit**

```bash
git add carina-provider-protocol/src/types.rs carina-plugin-host/src/wasm_convert.rs carina-plugin-host/src/wasm_factory.rs
git commit -m "feat: add version field to ProviderInfo protocol"
```

---

### Task 4: Update mock provider to report version

**Files:**
- Modify: `carina-provider-mock/src/lib.rs`

- [ ] **Step 1: Find where `ProviderInfo` is constructed in the mock provider**

Search for `ProviderInfo` construction in `carina-provider-mock/src/lib.rs`. The mock provider is a native Rust implementation (not WASM), so it implements the `Provider` trait directly. Check if it uses `ProviderInfo` from the protocol crate.

If the mock provider doesn't use `ProviderInfo` at all (it's native, not WASM), skip this task.

- [ ] **Step 2: If applicable, add version to the mock's ProviderInfo**

Add `version: env!("CARGO_PKG_VERSION").to_string()` to the `ProviderInfo` construction.

- [ ] **Step 3: Run tests**

Run: `cargo test -p carina-provider-mock`

Expected: All tests pass.

- [ ] **Step 4: Commit**

```bash
git add carina-provider-mock/
git commit -m "feat: mock provider reports version in ProviderInfo"
```

---

### Task 5: Add `constraint` field to `LockEntry` and update lock file operations

**Files:**
- Modify: `carina-cli/src/provider_resolver.rs`

- [ ] **Step 1: Write the failing test**

Add to `carina-cli/src/provider_resolver.rs` in the `tests` module:

```rust
    #[test]
    fn lock_entry_with_constraint_roundtrip() {
        let lock = LockFile {
            provider: vec![LockEntry {
                name: "aws".to_string(),
                source: "github.com/carina-rs/carina-provider-aws".to_string(),
                version: "0.5.2".to_string(),
                constraint: Some("~0.5.0".to_string()),
                sha256: "abc123".to_string(),
            }],
        };
        let toml_str = toml::to_string_pretty(&lock).unwrap();
        let loaded: LockFile = toml::from_str(&toml_str).unwrap();
        assert_eq!(loaded.provider[0].constraint.as_deref(), Some("~0.5.0"));
    }

    #[test]
    fn lock_entry_without_constraint_deserializes() {
        let toml_str = r#"
[[provider]]
name = "aws"
source = "github.com/carina-rs/carina-provider-aws"
version = "0.5.0"
sha256 = "abc123"
"#;
        let lock: LockFile = toml::from_str(toml_str).unwrap();
        assert!(lock.provider[0].constraint.is_none());
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p carina-cli lock_entry_with_constraint`

Expected: FAIL — `constraint` field doesn't exist.

- [ ] **Step 3: Add `constraint` field to `LockEntry`**

In `carina-cli/src/provider_resolver.rs` (around line 13):

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LockEntry {
    pub name: String,
    pub source: String,
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub constraint: Option<String>,
    pub sha256: String,
}
```

- [ ] **Step 4: Add `find_by_source` method to `LockFile`**

Add a new method alongside the existing `find` (around line 41):

```rust
    pub fn find_by_source(&self, source: &str) -> Option<&LockEntry> {
        self.provider.iter().find(|e| e.source == source)
    }
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p carina-cli lock_entry`

Expected: Both new tests pass.

- [ ] **Step 6: Commit**

```bash
git add carina-cli/src/provider_resolver.rs
git commit -m "feat: add constraint field to LockEntry"
```

---

### Task 6: Implement GitHub Releases version resolution

**Files:**
- Create: `carina-cli/src/version_resolver.rs`
- Modify: `carina-cli/src/main.rs` (add module declaration)
- Modify: `carina-cli/Cargo.toml`

- [ ] **Step 1: Add `semver` dependency to `carina-cli`**

In `carina-cli/Cargo.toml`, add to `[dependencies]`:

```toml
semver = "1"
```

- [ ] **Step 2: Write version resolver with tests**

Create `carina-cli/src/version_resolver.rs`:

```rust
//! Resolve version constraints against GitHub Releases API.

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
```

- [ ] **Step 3: Add module declaration**

In `carina-cli/src/main.rs`, add near the other `mod` declarations:

```rust
mod version_resolver;
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p carina-cli version_resolver`

Expected: All 7 tests pass.

- [ ] **Step 5: Commit**

```bash
git add carina-cli/src/version_resolver.rs carina-cli/src/main.rs carina-cli/Cargo.toml
git commit -m "feat: add version resolver with GitHub Releases API"
```

---

### Task 7: Integrate version resolution into `resolve_all` and `resolve_single_config`

**Files:**
- Modify: `carina-cli/src/provider_resolver.rs`

- [ ] **Step 1: Add imports**

At the top of `carina-cli/src/provider_resolver.rs`, add:

```rust
use carina_core::version_constraint::VersionConstraint;
```

- [ ] **Step 2: Add `resolve_version` helper function**

Add this function to `carina-cli/src/provider_resolver.rs` (before `resolve_all`):

```rust
/// Resolve the exact version to use for a provider.
///
/// Resolution order:
/// 1. If not upgrading and lock has a version satisfying the constraint, use it.
/// 2. If constraint specified, fetch releases from GitHub and find best match.
/// 3. If no constraint, fetch latest release from GitHub.
fn resolve_version(
    source: &str,
    config: &ProviderConfig,
    lock_file: &LockFile,
    upgrade: bool,
) -> Result<String, String> {
    // Check lock file first (unless upgrading)
    if !upgrade {
        if let Some(lock_entry) = lock_file.find_by_source(source) {
            if let Some(constraint) = &config.version {
                if constraint.matches(&lock_entry.version).unwrap_or(false) {
                    return Ok(lock_entry.version.clone());
                }
                // Lock doesn't satisfy constraint — will re-resolve below
            } else {
                // No constraint, use locked version
                return Ok(lock_entry.version.clone());
            }
        }
    }

    // Resolve from GitHub
    match &config.version {
        Some(constraint) => {
            let tags = crate::version_resolver::fetch_release_tags(source)?;
            let resolved = crate::version_resolver::resolve_from_tags(&tags, &constraint.req)
                .ok_or_else(|| {
                    format!(
                        "No release of '{}' matches constraint '{}'. Available: {}",
                        config.name,
                        constraint.raw,
                        tags.join(", ")
                    )
                })?;
            Ok(resolved.version.to_string())
        }
        None => {
            // No constraint — fetch latest
            let tag = crate::version_resolver::fetch_latest_tag(source)?;
            let version = tag.strip_prefix('v').unwrap_or(&tag);
            Ok(version.to_string())
        }
    }
}
```

- [ ] **Step 3: Update `resolve_all` to use version resolution**

Replace the `resolve_all` function (starting at line 365):

```rust
/// Resolve all providers that need GitHub source resolution.
pub fn resolve_all(
    base_dir: &Path,
    providers: &[ProviderConfig],
    upgrade: bool,
) -> Result<HashMap<String, PathBuf>, String> {
    let lock_path = base_dir.join("carina.lock");
    let mut lock_file = LockFile::load(&lock_path).unwrap_or_default();
    let mut resolved = HashMap::new();

    for config in providers {
        let source = match &config.source {
            Some(s) if !s.starts_with("file://") => s.as_str(),
            _ => continue,
        };

        let version = resolve_version(source, config, &lock_file, upgrade)?;

        let binary_path =
            resolve_provider(base_dir, source, &version, &config.name, &mut lock_file)?;

        // Store constraint in lock entry
        if let Some(entry) = lock_file.provider.iter_mut().find(|e| e.source == source) {
            entry.constraint = config.version.as_ref().map(|c| c.raw.clone());
        }

        resolved.insert(config.name.clone(), binary_path);
    }

    if !resolved.is_empty() {
        lock_file
            .save(&lock_path)
            .map_err(|e| format!("Failed to save carina.lock: {e}"))?;
    }

    Ok(resolved)
}
```

- [ ] **Step 4: Update `resolve_single_config` to use version resolution**

Replace the `resolve_single_config` function (starting at line 334):

```rust
/// Resolve a single provider config with lock file management.
pub fn resolve_single_config(base_dir: &Path, config: &ProviderConfig) -> Result<PathBuf, String> {
    let source = config
        .source
        .as_deref()
        .ok_or_else(|| format!("Provider '{}' has no source", config.name))?;

    let lock_path = base_dir.join("carina.lock");
    let mut lock_file = LockFile::load(&lock_path).unwrap_or_default();

    let version = resolve_version(source, config, &lock_file, false)?;

    let binary_path = resolve_provider(base_dir, source, &version, &config.name, &mut lock_file)?;

    if let Some(entry) = lock_file.provider.iter_mut().find(|e| e.source == source) {
        entry.constraint = config.version.as_ref().map(|c| c.raw.clone());
    }

    lock_file
        .save(&lock_path)
        .map_err(|e| format!("Failed to save carina.lock: {e}"))?;

    Ok(binary_path)
}
```

- [ ] **Step 5: Update all call sites of `resolve_all`**

Search for all call sites: `grep -rn "resolve_all" carina-cli/src/`

Update each to pass the `upgrade` parameter. In `commands/init.rs`, pass the function argument. In any other call sites, pass `false`.

- [ ] **Step 6: Fix compilation and run tests**

Run: `cargo check -p carina-cli && cargo test -p carina-cli`

Expected: Compiles and all tests pass.

- [ ] **Step 7: Commit**

```bash
git add carina-cli/src/provider_resolver.rs
git commit -m "feat: integrate version constraint resolution into provider resolver"
```

---

### Task 8: Add `--upgrade` flag to `carina init`

**Files:**
- Modify: `carina-cli/src/main.rs`
- Modify: `carina-cli/src/commands/init.rs`

- [ ] **Step 1: Add `--upgrade` flag to the `Init` command**

In `carina-cli/src/main.rs` (around line 162), update the `Init` variant:

```rust
    /// Download and install provider binaries
    Init {
        /// Path to directory containing .crn files
        #[arg(default_value = ".")]
        path: PathBuf,

        /// Re-resolve all provider versions from constraints, ignoring lock file
        #[arg(long)]
        upgrade: bool,
    },
```

- [ ] **Step 2: Pass `upgrade` to `run_init`**

In the `Init` handler (around line 303):

```rust
        Commands::Init { path, upgrade } => {
            if let Err(e) = commands::init::run_init(&path, upgrade) {
                eprintln!("{}", format!("Error: {e}").red());
                std::process::exit(1);
            }
            Ok(())
        }
```

- [ ] **Step 3: Update `run_init` to accept and pass `upgrade`**

In `carina-cli/src/commands/init.rs`, update the function:

```rust
pub fn run_init(path: &Path, upgrade: bool) -> Result<(), String> {
    let base_dir = get_base_dir(path);
    let path_buf = path.to_path_buf();

    let provider_context = ProviderContext::default();
    let loaded = load_configuration_with_config(&path_buf, &provider_context)
        .map_err(|e| format!("Failed to load configuration: {e}"))?;

    let github_providers: Vec<_> = loaded
        .parsed
        .providers
        .iter()
        .filter(|p| p.source.as_ref().is_some_and(|s| !s.starts_with("file://")))
        .collect();

    if github_providers.is_empty() {
        println!(
            "{}",
            "No providers with remote source found. Nothing to do.".cyan()
        );
        return Ok(());
    }

    let action = if upgrade { "Upgrading" } else { "Resolving" };
    println!(
        "{}",
        format!("{action} {} provider(s)...", github_providers.len()).cyan()
    );

    let resolved = provider_resolver::resolve_all(base_dir, &loaded.parsed.providers, upgrade)?;

    println!(
        "{}",
        format!(
            "Done. {} provider(s) installed in .carina/providers/",
            resolved.len()
        )
        .green()
    );

    Ok(())
}
```

- [ ] **Step 4: Fix compilation and run tests**

Run: `cargo check -p carina-cli && cargo test -p carina-cli`

Expected: Compiles and all tests pass.

- [ ] **Step 5: Commit**

```bash
git add carina-cli/src/main.rs carina-cli/src/commands/init.rs
git commit -m "feat: add --upgrade flag to carina init"
```

---

### Task 9: Add lock validation to `plan` and `apply`

**Files:**
- Modify: `carina-cli/src/provider_resolver.rs`
- Modify: `carina-cli/src/wiring.rs`

- [ ] **Step 1: Add lock validation function**

Add to `carina-cli/src/provider_resolver.rs`:

```rust
/// Validate that all locked provider versions satisfy their DSL constraints.
/// Returns an error if any provider's locked version doesn't match its constraint.
pub fn validate_lock_constraints(
    base_dir: &Path,
    providers: &[ProviderConfig],
) -> Result<(), String> {
    let lock_path = base_dir.join("carina.lock");
    let lock_file = match LockFile::load(&lock_path) {
        Some(lf) => lf,
        None => return Ok(()), // No lock file — nothing to validate
    };

    for config in providers {
        let source = match &config.source {
            Some(s) if !s.starts_with("file://") => s.as_str(),
            _ => continue,
        };

        let constraint = match &config.version {
            Some(c) => c,
            None => continue,
        };

        if let Some(lock_entry) = lock_file.find_by_source(source) {
            if !constraint.matches(&lock_entry.version).unwrap_or(false) {
                return Err(format!(
                    "Provider '{}' locked at version {}, but constraint '{}' requires a different version.\nRun `carina init --upgrade` to resolve.",
                    config.name, lock_entry.version, constraint.raw
                ));
            }
        }
    }

    Ok(())
}
```

- [ ] **Step 2: Call validation in `build_factories_from_providers`**

In `carina-cli/src/wiring.rs`, at the start of `build_factories_from_providers` (around line 77), add:

```rust
    // Validate lock file constraints before loading providers
    if let Err(e) = crate::provider_resolver::validate_lock_constraints(base_dir, providers) {
        eprintln!("{}", e.red());
        return Vec::new();
    }
```

- [ ] **Step 3: Fix compilation and run tests**

Run: `cargo check -p carina-cli && cargo test -p carina-cli`

Expected: Compiles and all tests pass.

- [ ] **Step 4: Commit**

```bash
git add carina-cli/src/provider_resolver.rs carina-cli/src/wiring.rs
git commit -m "feat: validate lock file constraints before plan/apply"
```

---

### Task 10: Add load-time version verification in plugin host

**Files:**
- Modify: `carina-plugin-host/Cargo.toml`
- Modify: `carina-plugin-host/src/wasm_factory.rs`
- Modify: `carina-cli/src/wiring.rs`

- [ ] **Step 1: Add `semver` dependency to `carina-plugin-host`**

In `carina-plugin-host/Cargo.toml`, add to `[dependencies]`:

```toml
semver = "1"
```

- [ ] **Step 2: Add version verification method to `WasmProviderFactory`**

In `carina-plugin-host/src/wasm_factory.rs`, add a method to the `impl WasmProviderFactory` block:

```rust
    /// Verify that this provider's version satisfies the given constraint.
    pub fn verify_version(&self, constraint_raw: &str) -> Result<(), String> {
        let req = semver::VersionReq::parse(constraint_raw)
            .map_err(|e| format!("Invalid version constraint '{}': {}", constraint_raw, e))?;
        let actual = semver::Version::parse(&self.version)
            .map_err(|e| format!("Provider '{}' reports invalid version '{}': {}", self.name_static, self.version, e))?;
        if !req.matches(&actual) {
            return Err(format!(
                "Provider '{}' version {} does not satisfy constraint '{}'",
                self.name_static, actual, constraint_raw
            ));
        }
        Ok(())
    }
```

- [ ] **Step 3: Call verification in `wiring.rs` after factory creation**

In `carina-cli/src/wiring.rs`, in the `Ok(factory)` match arm of `build_factories_from_providers` (around line 138), add verification before pushing the factory. The exact approach depends on whether `ProviderFactory` trait supports downcasting. Check the trait definition and use the appropriate method:

Option A — If `ProviderFactory` has `as_any()`:
```rust
            Ok(factory) => {
                if let Some(constraint) = &config.version {
                    if let Some(wasm_factory) = factory.as_any().downcast_ref::<carina_plugin_host::WasmProviderFactory>() {
                        if let Err(e) = wasm_factory.verify_version(&constraint.raw) {
                            eprintln!("{}", e.red());
                            continue;
                        }
                    }
                }
                factories.push(factory);
            }
```

Option B — If downcasting isn't available, verify before boxing:
```rust
        let factory_result: Result<Box<dyn ProviderFactory>, String> = {
            tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(
                    carina_plugin_host::WasmProviderFactory::new(binary_path.clone()),
                )
            })
            .and_then(|f| {
                // Verify version constraint before boxing
                if let Some(constraint) = &config.version {
                    f.verify_version(&constraint.raw)?;
                }
                Ok(Box::new(f) as Box<dyn ProviderFactory>)
            })
            .map_err(|e| format!("Failed to load WASM provider: {e}"))
        };
```

Option B is preferred as it avoids needing `as_any()` on the trait.

- [ ] **Step 4: Fix compilation and run tests**

Run: `cargo check -p carina-plugin-host && cargo check -p carina-cli && cargo test -p carina-cli`

Expected: Compiles and all tests pass.

- [ ] **Step 5: Commit**

```bash
git add carina-plugin-host/Cargo.toml carina-plugin-host/src/wasm_factory.rs carina-cli/src/wiring.rs
git commit -m "feat: verify provider version at load time"
```

---

### Task 11: End-to-end validation with fixture

**Files:**
- Create: `carina-cli/tests/fixtures/version_constraint/main.crn`

- [ ] **Step 1: Create a fixture `.crn` file with version constraints**

Create `carina-cli/tests/fixtures/version_constraint/main.crn`:

```
provider mock {
    source = "github.com/carina-rs/carina-provider-mock"
    version = "~0.1.0"
}
```

- [ ] **Step 2: Verify the fixture parses correctly**

Run: `cargo run -- validate carina-cli/tests/fixtures/version_constraint/`

Expected: Validation succeeds or fails with a provider-not-found error — but NOT a parse error. The constraint syntax `~0.1.0` must be accepted by the parser.

- [ ] **Step 3: Run full workspace tests**

Run: `cargo test`

Expected: All tests across all crates pass.

- [ ] **Step 4: Commit**

```bash
git add carina-cli/tests/fixtures/version_constraint/
git commit -m "test: add version constraint fixture for e2e validation"
```
