# Phase 2b: `carina init` + Provider Auto-Resolution — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add `carina init` command and automatic provider resolution so that providers with `source = "github.com/..."` are downloaded from GitHub Releases, cached in `.carina/providers/`, and verified via `carina.lock`.

**Architecture:** A new `provider_resolver` module in `carina-cli` handles download, extraction, caching, and lock file management. The `carina init` command calls this module explicitly; `plan`/`apply` call it automatically when a provider binary is missing. The existing `load_process_provider()` in `wiring.rs` is updated to resolve GitHub sources before spawning.

**Tech Stack:** `ureq` (sync HTTP), `flate2` (gzip), `tar` (tar extraction), `sha2` (SHA256), `toml` (lock file format).

**Spec:** `docs/superpowers/specs/2026-03-31-carina-init-design.md`

---

## File Structure

### New Files

```
carina-cli/src/provider_resolver.rs     — Provider resolution: download, extract, cache, lock file, platform detection
carina-cli/src/commands/init.rs         — `carina init` command handler
```

### Modified Files

```
carina-cli/Cargo.toml                   — Add ureq, flate2, tar, sha2, toml dependencies
carina-cli/src/main.rs                  — Add Init variant to Commands enum
carina-cli/src/commands/mod.rs          — Add init module
carina-cli/src/wiring.rs               — Replace GitHub source TODO with resolver call
```

---

## Task 1: Add dependencies to `carina-cli/Cargo.toml`

**Files:**
- Modify: `carina-cli/Cargo.toml`

- [ ] **Step 1: Add new dependencies**

Add these to `[dependencies]` in `carina-cli/Cargo.toml`:

```toml
flate2 = "1"
sha2 = "0.10"
tar = "0.4"
toml = "0.8"
ureq = "3"
```

- [ ] **Step 2: Verify build**

Run: `cargo check -p carina-cli`
Expected: CHECK SUCCESS

- [ ] **Step 3: Commit**

```bash
git add carina-cli/Cargo.toml
git commit -m "chore: add ureq, flate2, tar, sha2, toml dependencies for provider resolver"
```

---

## Task 2: Create `provider_resolver.rs` with platform detection, URL construction, and lock file

This is the core module. It handles everything: platform detection, URL construction, download, extraction, SHA256 verification, lock file read/write, and the resolve orchestration.

**Files:**
- Create: `carina-cli/src/provider_resolver.rs`
- Modify: `carina-cli/src/main.rs` (add `mod provider_resolver;`)

- [ ] **Step 1: Create `provider_resolver.rs` with types and platform detection**

Create `carina-cli/src/provider_resolver.rs`:

```rust
//! Provider resolution: download, extract, cache, and verify provider binaries.

use std::collections::HashMap;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use carina_core::parser::ProviderConfig;

/// A single provider entry in carina.lock.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LockEntry {
    pub name: String,
    pub source: String,
    pub version: String,
    pub sha256: String,
}

/// The full carina.lock file.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LockFile {
    #[serde(default)]
    pub provider: Vec<LockEntry>,
}

impl LockFile {
    pub fn load(path: &Path) -> Option<Self> {
        let content = fs::read_to_string(path).ok()?;
        toml::from_str(&content).ok()
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        let content = toml::to_string_pretty(self).map_err(|e| {
            io::Error::new(io::ErrorKind::Other, format!("Failed to serialize lock file: {e}"))
        })?;
        fs::write(path, content)
    }

    pub fn find(&self, source: &str, version: &str) -> Option<&LockEntry> {
        self.provider
            .iter()
            .find(|e| e.source == source && e.version == version)
    }

    pub fn upsert(&mut self, entry: LockEntry) {
        if let Some(existing) = self
            .provider
            .iter_mut()
            .find(|e| e.source == entry.source)
        {
            *existing = entry;
        } else {
            self.provider.push(entry);
        }
    }
}

/// Detect the current platform's target triple.
///
/// Maps `std::env::consts::{OS, ARCH}` to Rust target triple format.
pub fn detect_target() -> Result<String, String> {
    let arch = std::env::consts::ARCH;
    let os = std::env::consts::OS;

    let target = match (arch, os) {
        ("aarch64", "macos") => "aarch64-apple-darwin",
        ("x86_64", "macos") => "x86_64-apple-darwin",
        ("x86_64", "linux") => "x86_64-unknown-linux-gnu",
        ("aarch64", "linux") => "aarch64-unknown-linux-gnu",
        _ => return Err(format!("Unsupported platform: {arch}-{os}")),
    };

    Ok(target.to_string())
}

/// Construct the download URL for a provider binary.
///
/// Format: `https://github.com/{owner}/{repo}/releases/download/v{version}/{repo}-v{version}-{target}.tar.gz`
pub fn download_url(source: &str, version: &str, target: &str) -> Result<String, String> {
    // source is "github.com/{owner}/{repo}"
    let parts: Vec<&str> = source.split('/').collect();
    if parts.len() != 3 || parts[0] != "github.com" {
        return Err(format!(
            "Invalid source format: {source}. Expected: github.com/{{owner}}/{{repo}}"
        ));
    }
    let owner = parts[1];
    let repo = parts[2];

    Ok(format!(
        "https://github.com/{owner}/{repo}/releases/download/v{version}/{repo}-v{version}-{target}.tar.gz"
    ))
}

/// Resolve the cache path for a provider binary.
///
/// Returns: `{base_dir}/.carina/providers/{source}/{version}/{binary_name}`
pub fn cache_path(base_dir: &Path, source: &str, version: &str) -> PathBuf {
    let repo = source.split('/').last().unwrap_or("provider");
    base_dir
        .join(".carina")
        .join("providers")
        .join(source)
        .join(version)
        .join(repo)
}

/// Compute SHA256 hex digest of a file.
pub fn sha256_file(path: &Path) -> io::Result<String> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 8192];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// Download a file from a URL and save it to a path.
fn download_to_file(url: &str, dest: &Path) -> Result<(), String> {
    let response = ureq::get(url)
        .call()
        .map_err(|e| format!("Failed to download {url}: {e}"))?;

    if response.status() != 200 {
        return Err(format!(
            "Download failed with status {}: {url}",
            response.status()
        ));
    }

    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create directory {}: {e}", parent.display()))?;
    }

    let mut reader = response.into_body().into_reader();
    let mut file = fs::File::create(dest)
        .map_err(|e| format!("Failed to create file {}: {e}", dest.display()))?;
    io::copy(&mut reader, &mut file)
        .map_err(|e| format!("Failed to write file {}: {e}", dest.display()))?;

    Ok(())
}

/// Extract a tar.gz archive. Returns the path to the extracted binary.
fn extract_tar_gz(archive_path: &Path, dest_dir: &Path) -> Result<PathBuf, String> {
    let file = fs::File::open(archive_path)
        .map_err(|e| format!("Failed to open archive {}: {e}", archive_path.display()))?;
    let decoder = flate2::read::GzDecoder::new(file);
    let mut archive = tar::Archive::new(decoder);

    fs::create_dir_all(dest_dir)
        .map_err(|e| format!("Failed to create dir {}: {e}", dest_dir.display()))?;

    archive
        .unpack(dest_dir)
        .map_err(|e| format!("Failed to extract archive: {e}"))?;

    // Find the binary in the extracted directory (should be at top level)
    let entries = fs::read_dir(dest_dir)
        .map_err(|e| format!("Failed to read dir {}: {e}", dest_dir.display()))?;

    for entry in entries {
        let entry = entry.map_err(|e| format!("Failed to read entry: {e}"))?;
        let path = entry.path();
        if path.is_file() && !path.extension().is_some_and(|ext| ext == "gz" || ext == "tar") {
            // Make executable on Unix
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mut perms = fs::metadata(&path)
                    .map_err(|e| format!("Failed to read metadata: {e}"))?
                    .permissions();
                perms.set_mode(0o755);
                fs::set_permissions(&path, perms)
                    .map_err(|e| format!("Failed to set permissions: {e}"))?;
            }
            return Ok(path);
        }
    }

    Err(format!(
        "No binary found in archive: {}",
        archive_path.display()
    ))
}

/// Resolve a single provider: download if missing, verify if cached.
///
/// Returns the path to the provider binary.
pub fn resolve_provider(
    base_dir: &Path,
    source: &str,
    version: &str,
    name: &str,
    lock_file: &mut LockFile,
) -> Result<PathBuf, String> {
    let binary_path = cache_path(base_dir, source, version);

    if binary_path.exists() {
        // Binary exists — verify against lock file if available
        if let Some(lock_entry) = lock_file.find(source, version) {
            let actual_hash = sha256_file(&binary_path)
                .map_err(|e| format!("Failed to hash binary: {e}"))?;
            if actual_hash != lock_entry.sha256 {
                return Err(format!(
                    "SHA256 mismatch for provider '{}' ({}@{}). Expected: {}, got: {}. Re-run `carina init` to re-download.",
                    name, source, version, lock_entry.sha256, actual_hash
                ));
            }
        }
        return Ok(binary_path);
    }

    // Binary missing — download
    let target = detect_target()?;
    let url = download_url(source, version, &target)?;

    println!("Downloading provider '{}' from {}", name, url);

    let tmp_archive = base_dir
        .join(".carina")
        .join("providers")
        .join("tmp_download.tar.gz");

    download_to_file(&url, &tmp_archive)?;

    // Extract to cache directory
    let dest_dir = binary_path.parent().unwrap();
    let extracted = extract_tar_gz(&tmp_archive, dest_dir)?;

    // Clean up archive
    let _ = fs::remove_file(&tmp_archive);

    // If extracted file has a different name, rename to expected name
    if extracted != binary_path {
        fs::rename(&extracted, &binary_path).map_err(|e| {
            format!(
                "Failed to rename {} to {}: {e}",
                extracted.display(),
                binary_path.display()
            )
        })?;
    }

    // Compute SHA256 and update lock file
    let hash = sha256_file(&binary_path)
        .map_err(|e| format!("Failed to hash binary: {e}"))?;

    lock_file.upsert(LockEntry {
        name: name.to_string(),
        source: source.to_string(),
        version: version.to_string(),
        sha256: hash,
    });

    println!("Installed provider '{}' ({}@{})", name, source, version);

    Ok(binary_path)
}

/// Resolve all providers that need GitHub source resolution.
///
/// Called by `carina init` and auto-resolution in plan/apply.
pub fn resolve_all(
    base_dir: &Path,
    providers: &[ProviderConfig],
) -> Result<HashMap<String, PathBuf>, String> {
    let lock_path = base_dir.join("carina.lock");
    let mut lock_file = LockFile::load(&lock_path).unwrap_or_default();
    let mut resolved = HashMap::new();

    for config in providers {
        let source = match &config.source {
            Some(s) if !s.starts_with("file://") => s.as_str(),
            _ => continue, // Skip non-GitHub sources
        };

        let version = config.version.as_deref().ok_or_else(|| {
            format!(
                "Provider '{}' has source but no version. Add: version = \"x.y.z\"",
                config.name
            )
        })?;

        let binary_path = resolve_provider(base_dir, source, version, &config.name, &mut lock_file)?;
        resolved.insert(config.name.clone(), binary_path);
    }

    // Save lock file if any providers were resolved
    if !resolved.is_empty() {
        lock_file
            .save(&lock_path)
            .map_err(|e| format!("Failed to save carina.lock: {e}"))?;
    }

    Ok(resolved)
}
```

- [ ] **Step 2: Add `mod provider_resolver;` to main.rs**

In `carina-cli/src/main.rs`, add near the top with other module declarations:

```rust
mod provider_resolver;
```

- [ ] **Step 3: Verify build**

Run: `cargo check -p carina-cli`
Expected: CHECK SUCCESS

- [ ] **Step 4: Write tests for platform detection, URL construction, and lock file**

Add at the bottom of `carina-cli/src/provider_resolver.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn test_detect_target() {
        let target = detect_target().unwrap();
        // Should return a valid target for the current platform
        assert!(
            target.contains("apple-darwin") || target.contains("unknown-linux"),
            "Unexpected target: {target}"
        );
    }

    #[test]
    fn test_download_url() {
        let url = download_url(
            "github.com/carina-rs/carina-provider-awscc",
            "0.1.0",
            "aarch64-apple-darwin",
        )
        .unwrap();
        assert_eq!(
            url,
            "https://github.com/carina-rs/carina-provider-awscc/releases/download/v0.1.0/carina-provider-awscc-v0.1.0-aarch64-apple-darwin.tar.gz"
        );
    }

    #[test]
    fn test_download_url_invalid_source() {
        let result = download_url("invalid-source", "0.1.0", "x86_64-unknown-linux-gnu");
        assert!(result.is_err());
    }

    #[test]
    fn test_cache_path() {
        let base = Path::new("/tmp/project");
        let path = cache_path(
            base,
            "github.com/carina-rs/carina-provider-awscc",
            "0.1.0",
        );
        assert_eq!(
            path,
            PathBuf::from("/tmp/project/.carina/providers/github.com/carina-rs/carina-provider-awscc/0.1.0/carina-provider-awscc")
        );
    }

    #[test]
    fn test_lock_file_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("carina.lock");

        let mut lock = LockFile::default();
        lock.upsert(LockEntry {
            name: "awscc".into(),
            source: "github.com/carina-rs/carina-provider-awscc".into(),
            version: "0.1.0".into(),
            sha256: "abc123".into(),
        });

        lock.save(&lock_path).unwrap();
        let loaded = LockFile::load(&lock_path).unwrap();

        assert_eq!(loaded.provider.len(), 1);
        assert_eq!(loaded.provider[0].name, "awscc");
        assert_eq!(loaded.provider[0].sha256, "abc123");
    }

    #[test]
    fn test_lock_file_upsert_replaces_existing() {
        let mut lock = LockFile::default();
        lock.upsert(LockEntry {
            name: "awscc".into(),
            source: "github.com/carina-rs/carina-provider-awscc".into(),
            version: "0.1.0".into(),
            sha256: "old_hash".into(),
        });
        lock.upsert(LockEntry {
            name: "awscc".into(),
            source: "github.com/carina-rs/carina-provider-awscc".into(),
            version: "0.2.0".into(),
            sha256: "new_hash".into(),
        });

        assert_eq!(lock.provider.len(), 1);
        assert_eq!(lock.provider[0].version, "0.2.0");
        assert_eq!(lock.provider[0].sha256, "new_hash");
    }

    #[test]
    fn test_sha256_file() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("test.bin");
        let mut file = fs::File::create(&file_path).unwrap();
        file.write_all(b"hello world").unwrap();

        let hash = sha256_file(&file_path).unwrap();
        // SHA256("hello world") = b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9
        assert_eq!(
            hash,
            "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
        );
    }
}
```

- [ ] **Step 5: Add `tempfile` dev-dependency**

In `carina-cli/Cargo.toml`, add:

```toml
[dev-dependencies]
tempfile = "3"
```

- [ ] **Step 6: Run tests**

Run: `cargo test -p carina-cli test_detect_target test_download_url test_cache_path test_lock_file test_sha256`
Expected: All tests PASS

- [ ] **Step 7: Commit**

```bash
git add carina-cli/src/provider_resolver.rs carina-cli/src/main.rs carina-cli/Cargo.toml
git commit -m "feat: add provider_resolver module with download, cache, and lock file support"
```

---

## Task 3: Add `carina init` command

**Files:**
- Create: `carina-cli/src/commands/init.rs`
- Modify: `carina-cli/src/commands/mod.rs`
- Modify: `carina-cli/src/main.rs`

- [ ] **Step 1: Create `carina-cli/src/commands/init.rs`**

```rust
use std::path::Path;

use colored::Colorize;

use carina_core::config_loader::{get_base_dir, load_configuration_with_config};
use carina_core::parser::ProviderContext;

use crate::provider_resolver;

pub fn run_init(path: &Path) -> Result<(), String> {
    let base_dir = get_base_dir(path);

    // Load configuration to find provider declarations
    let provider_context = ProviderContext::default();
    let parsed = load_configuration_with_config(path, &provider_context)
        .map_err(|e| format!("Failed to load configuration: {e}"))?;

    let github_providers: Vec<_> = parsed
        .providers
        .iter()
        .filter(|p| {
            p.source
                .as_ref()
                .is_some_and(|s| !s.starts_with("file://"))
        })
        .collect();

    if github_providers.is_empty() {
        println!(
            "{}",
            "No providers with remote source found. Nothing to do.".cyan()
        );
        return Ok(());
    }

    println!(
        "{}",
        format!("Resolving {} provider(s)...", github_providers.len()).cyan()
    );

    let resolved = provider_resolver::resolve_all(base_dir, &parsed.providers)?;

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

- [ ] **Step 2: Add `init` module to `commands/mod.rs`**

In `carina-cli/src/commands/mod.rs`, add:

```rust
pub mod init;
```

- [ ] **Step 3: Add `Init` variant to `Commands` enum in `main.rs`**

In `carina-cli/src/main.rs`, add to the `Commands` enum:

```rust
    /// Download and install provider binaries
    Init {
        /// Path to .crn file or directory
        #[arg(default_value = ".")]
        path: PathBuf,
    },
```

- [ ] **Step 4: Add match arm for `Init` in `main()`**

In `carina-cli/src/main.rs`, in the match on `cli.command`, add:

```rust
        Commands::Init { path } => {
            if let Err(e) = commands::init::run_init(&path) {
                eprintln!("{}", format!("Error: {e}").red());
                std::process::exit(1);
            }
        }
```

- [ ] **Step 5: Verify build**

Run: `cargo build -p carina-cli`
Expected: BUILD SUCCESS

- [ ] **Step 6: Verify help output**

Run: `cargo run --bin carina -- init --help`
Expected: Shows help for init command with `path` argument

- [ ] **Step 7: Commit**

```bash
git add carina-cli/src/commands/init.rs carina-cli/src/commands/mod.rs carina-cli/src/main.rs
git commit -m "feat: add carina init command for provider installation"
```

---

## Task 4: Wire auto-resolution into `wiring.rs`

**Files:**
- Modify: `carina-cli/src/wiring.rs`

- [ ] **Step 1: Update `load_process_provider` to handle GitHub sources**

In `carina-cli/src/wiring.rs`, replace the `load_process_provider` function. The current function rejects non-`file://` sources with a TODO comment. Update it to resolve GitHub sources via `provider_resolver`:

```rust
async fn load_process_provider(
    source: &str,
    config: &ProviderConfig,
    base_dir: &Path,
) -> Result<
    (
        carina_plugin_host::ProcessProviderFactory,
        Box<dyn Provider>,
        String,
    ),
    String,
> {
    let binary_path = if let Some(path) = source.strip_prefix("file://") {
        std::path::PathBuf::from(path)
    } else if source.starts_with("github.com/") {
        // Resolve from .carina/providers/ cache (download if missing)
        let version = config.version.as_deref().ok_or_else(|| {
            format!(
                "Provider '{}' has source but no version. Add: version = \"x.y.z\"",
                config.name
            )
        })?;

        let lock_path = base_dir.join("carina.lock");
        let mut lock_file = crate::provider_resolver::LockFile::load(&lock_path)
            .unwrap_or_default();

        let path = crate::provider_resolver::resolve_provider(
            base_dir,
            source,
            version,
            &config.name,
            &mut lock_file,
        )?;

        // Save lock file if it was updated
        lock_file
            .save(&lock_path)
            .map_err(|e| format!("Failed to save carina.lock: {e}"))?;

        path
    } else {
        return Err(format!(
            "Unsupported source format: {source}. Use file:// for local binaries or github.com/owner/repo for remote."
        ));
    };

    let factory = carina_plugin_host::ProcessProviderFactory::new(binary_path)?;
    let name = factory.name().to_string();

    factory
        .validate_config(&config.attributes)
        .map_err(|e| format!("Config validation failed: {e}"))?;

    let provider = factory.create_provider(&config.attributes).await;
    Ok((factory, provider, name))
}
```

- [ ] **Step 2: Update callers to pass `base_dir`**

The `try_add_process_provider` function and its callers need to pass `base_dir`. Update `try_add_process_provider`:

```rust
async fn try_add_process_provider(
    router: &mut ProviderRouter,
    source: &str,
    config: &ProviderConfig,
    base_dir: &Path,
) {
    match load_process_provider(source, config, base_dir).await {
        // ... rest unchanged
    }
}
```

Update `get_provider_with_ctx` to accept and pass `base_dir`:

```rust
pub async fn get_provider_with_ctx(
    ctx: &WiringContext,
    parsed: &ParsedFile,
    base_dir: &Path,
) -> ProviderRouter {
```

And update the process provider call inside to pass `base_dir`:

```rust
        if let Some(ref source) = provider_config.source {
            try_add_process_provider(&mut router, source, provider_config, base_dir).await;
            continue;
        }
```

Similarly update `create_providers_from_configs` to accept `base_dir` and pass it.

- [ ] **Step 3: Update all callers of `get_provider_with_ctx`**

Search for all callers of `get_provider_with_ctx` in `carina-cli/src/commands/` and update them to pass `base_dir`. The `base_dir` is typically available via `get_base_dir(path)` which is already called in each command.

- [ ] **Step 4: Verify build**

Run: `cargo build -p carina-cli`
Expected: BUILD SUCCESS

- [ ] **Step 5: Run all tests**

Run: `cargo test -p carina-cli`
Expected: All tests PASS

- [ ] **Step 6: Commit**

```bash
git add carina-cli/src/wiring.rs carina-cli/src/commands/
git commit -m "feat: wire provider auto-resolution into plan/apply for GitHub sources"
```

---

## Task 5: Validate — full workspace build and test

**Files:**
- Potentially any file from Tasks 1-4

- [ ] **Step 1: Build all crates**

```bash
cargo build
```

Fix any compilation errors.

- [ ] **Step 2: Run all tests**

```bash
cargo test
```

Fix any test failures.

- [ ] **Step 3: Test `carina init` with a `.crn` that has no remote sources**

Create `/tmp/test-init.crn`:
```crn
provider awscc {
    region = awscc.Region.ap_northeast_1
}
```

Run:
```bash
cargo run --bin carina -- init /tmp/test-init.crn
```

Expected: "No providers with remote source found. Nothing to do."

- [ ] **Step 4: Test `carina init` with a remote source (expect download failure)**

Create `/tmp/test-init-remote.crn`:
```crn
provider awscc {
    source = "github.com/carina-rs/carina-provider-awscc"
    version = "0.1.0"
    region = awscc.Region.ap_northeast_1
}
```

Run:
```bash
cargo run --bin carina -- init /tmp/test-init-remote.crn
```

Expected: Error message about download failure (since the release doesn't exist yet). The error should be clear and mention the URL.

- [ ] **Step 5: Commit any fixes**

```bash
git add -A
git commit -m "fix: resolve issues in provider auto-resolution pipeline"
```

---

## Summary

| Task | Description | Key Output |
|------|-------------|------------|
| 1 | Add dependencies | ureq, flate2, tar, sha2, toml in Cargo.toml |
| 2 | Provider resolver module | Download, extract, cache, lock file, platform detection + tests |
| 3 | `carina init` command | New CLI subcommand calling resolver |
| 4 | Wire into plan/apply | `load_process_provider` resolves GitHub sources automatically |
| 5 | Validate | Full workspace build and test |
