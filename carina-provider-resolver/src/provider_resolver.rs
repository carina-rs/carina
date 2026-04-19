//! Provider resolution: download, extract, cache, and verify provider binaries.

use std::collections::HashMap;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use carina_core::parser::ProviderConfig;

/// A single provider entry in carina-providers.lock.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LockEntry {
    pub name: String,
    pub source: String,
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub constraint: Option<String>,
    /// Git revision (branch, tag, or commit SHA) specified in the provider block.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
    /// Resolved commit SHA for revision-based providers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_sha: Option<String>,
    pub sha256: String,
}

impl LockEntry {
    fn validate(&self) -> Result<(), String> {
        let has_version = !self.version.trim().is_empty();
        let has_revision = self.revision.is_some();
        let has_resolved_sha = self.resolved_sha.is_some();

        // file:// sources use the literal "file" as version; no revision/sha.
        if self.version == "file" {
            if has_revision || has_resolved_sha {
                return Err(format!(
                    "entry for '{}' is malformed: file:// provider must not have revision or resolved_sha",
                    self.name
                ));
            }
            return Ok(());
        }

        match (has_version, has_revision, has_resolved_sha) {
            // Version mode: version set, nothing else.
            (true, false, false) => Ok(()),
            // Revision mode: revision + resolved_sha set, version empty (filler).
            (false, true, true) => Ok(()),
            _ => Err(format!(
                "entry for '{}' is malformed (version={:?}, revision={:?}, resolved_sha={:?}): expected either version-mode (version set, no revision) or revision-mode (revision and resolved_sha both set, version empty)",
                self.name, self.version, self.revision, self.resolved_sha
            )),
        }
    }
}

/// The full carina-providers.lock file.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LockFile {
    #[serde(default)]
    pub provider: Vec<LockEntry>,
}

impl LockFile {
    /// Load and validate `carina-providers.lock`.
    ///
    /// Returns `Ok(None)` when the file is absent (normal first-run case),
    /// `Ok(Some(lock))` when the file parses and every entry is self-consistent,
    /// and `Err` when the file is present but unreadable, unparseable, or
    /// contains a malformed entry. Collapsing parse errors into "treat as
    /// empty" was how issue #2028 sneaked past the resolver — an entry with
    /// `version = ""` but `revision = "main"` was reused as if it were
    /// version-mode and produced `.../download/v/...` URLs.
    pub fn load(path: &Path) -> Result<Option<Self>, String> {
        let content = match fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("Failed to read {}: {e}", path.display())),
        };
        let lock: Self = toml::from_str(&content)
            .map_err(|e| format!("Failed to parse {}: {e}", path.display()))?;
        lock.validate().map_err(|e| {
            format!(
                "{}: {e}\nhint: delete {} and re-run `carina init`, or `carina init --upgrade`",
                path.display(),
                path.display()
            )
        })?;
        Ok(Some(lock))
    }

    /// Reject lock entries that carry logically impossible field combinations.
    ///
    /// A valid entry is one of:
    /// - **Version mode**: `version` non-empty; `revision` and `resolved_sha` absent.
    /// - **Revision mode**: `version` empty (filler); `revision` and `resolved_sha` both set.
    /// - **file:// source**: `version` is the literal string `"file"`;
    ///   `revision` and `resolved_sha` absent. Kept separate because file
    ///   sources bypass GitHub releases entirely.
    ///
    /// Anything else — empty `version` without a `revision`, `revision` without
    /// `resolved_sha`, both modes half-populated — is rejected.
    fn validate(&self) -> Result<(), String> {
        for entry in &self.provider {
            entry.validate()?;
        }
        Ok(())
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        let content = toml::to_string_pretty(self)
            .map_err(|e| io::Error::other(format!("Failed to serialize lock file: {e}")))?;
        fs::write(path, content)
    }

    pub fn find(&self, source: &str, version: &str) -> Option<&LockEntry> {
        self.provider
            .iter()
            .find(|e| e.source == source && e.version == version)
    }

    pub fn find_by_source(&self, source: &str) -> Option<&LockEntry> {
        self.provider.iter().find(|e| e.source == source)
    }

    pub fn find_by_source_and_sha(&self, source: &str, sha: &str) -> Option<&LockEntry> {
        self.provider
            .iter()
            .find(|e| e.source == source && e.resolved_sha.as_deref() == Some(sha))
    }

    pub fn upsert(&mut self, entry: LockEntry) {
        if let Some(existing) = self.provider.iter_mut().find(|e| e.source == entry.source) {
            *existing = entry;
        } else {
            self.provider.push(entry);
        }
    }
}

/// Detect the current platform's target triple.
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

/// Reject obviously bogus version strings before they get spliced into a URL
/// template. Empty or whitespace-only values were producing `.../download/v/...`
/// URLs that 404 and confused users into chasing release-side bugs (#2028).
fn check_version_for_url(version: &str) -> Result<(), String> {
    if version.trim().is_empty() {
        return Err("refusing to build download URL with empty version string. \
             This usually means carina-providers.lock is malformed — \
             delete it and re-run `carina init`, or run `carina init --upgrade`."
            .to_string());
    }
    Ok(())
}

/// Construct the download URL for a provider binary.
pub fn download_url(source: &str, version: &str, target: &str) -> Result<String, String> {
    check_version_for_url(version)?;
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

/// Construct the download URL for a WASM provider binary.
pub fn download_url_wasm(source: &str, version: &str) -> Result<String, String> {
    check_version_for_url(version)?;
    let parts: Vec<&str> = source.split('/').collect();
    if parts.len() != 3 || parts[0] != "github.com" {
        return Err(format!(
            "Invalid source format: {source}. Expected: github.com/{{owner}}/{{repo}}"
        ));
    }
    let owner = parts[1];
    let repo = parts[2];

    Ok(format!(
        "https://github.com/{owner}/{repo}/releases/download/v{version}/{repo}-v{version}.wasm"
    ))
}

/// Get the global plugin cache directory.
///
/// Checks `CARINA_PLUGIN_CACHE_DIR` environment variable first,
/// then falls back to `~/.carina/plugin-cache/`.
/// Returns `None` if the home directory cannot be determined.
pub fn global_cache_dir() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("CARINA_PLUGIN_CACHE_DIR") {
        return Some(PathBuf::from(dir));
    }
    dirs::home_dir().map(|home| home.join(".carina").join("plugin-cache"))
}

/// Resolve the global cache path for a WASM provider.
fn global_cache_path_wasm(source: &str, version: &str) -> Option<PathBuf> {
    let repo = source.split('/').next_back().unwrap_or("provider");
    global_cache_dir().map(|dir| dir.join(source).join(version).join(format!("{repo}.wasm")))
}

/// Resolve the cache path for a provider binary.
pub fn cache_path(base_dir: &Path, source: &str, version: &str) -> PathBuf {
    let repo = source.split('/').next_back().unwrap_or("provider");
    base_dir
        .join(".carina")
        .join("providers")
        .join(source)
        .join(version)
        .join(repo)
}

/// Resolve the cache path for a WASM provider binary.
pub fn cache_path_wasm(base_dir: &Path, source: &str, version: &str) -> PathBuf {
    let repo = source.split('/').next_back().unwrap_or("provider");
    base_dir
        .join(".carina")
        .join("providers")
        .join(source)
        .join(version)
        .join(format!("{repo}.wasm"))
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

    // Find the binary in the extracted directory
    let entries = fs::read_dir(dest_dir)
        .map_err(|e| format!("Failed to read dir {}: {e}", dest_dir.display()))?;

    for entry in entries {
        let entry = entry.map_err(|e| format!("Failed to read entry: {e}"))?;
        let path = entry.path();
        if path.is_file()
            && !path
                .extension()
                .is_some_and(|ext| ext == "gz" || ext == "tar")
        {
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
/// Resolution order:
/// 1. Check WASM cache — use it if it exists (after SHA256 verification).
/// 2. Check native binary cache — use it if it exists (after SHA256 verification).
/// 3. Try downloading WASM first (platform-independent).
/// 4. Fall back to downloading the native binary as a tar.gz.
pub fn resolve_provider(
    base_dir: &Path,
    source: &str,
    version: &str,
    name: &str,
    lock_file: &mut LockFile,
) -> Result<PathBuf, String> {
    // 1. Check local WASM cache first.
    let wasm_path = cache_path_wasm(base_dir, source, version);
    if wasm_path.exists() {
        if let Some(lock_entry) = lock_file.find(source, version) {
            let actual_hash =
                sha256_file(&wasm_path).map_err(|e| format!("Failed to hash WASM binary: {e}"))?;
            if actual_hash != lock_entry.sha256 {
                return Err(format!(
                    "SHA256 mismatch for provider '{}' ({}@{}). Expected: {}, got: {}. Re-run `carina init` to re-download.",
                    name, source, version, lock_entry.sha256, actual_hash
                ));
            }
        }
        return Ok(wasm_path);
    }

    // 2. Check native binary cache.
    let binary_path = cache_path(base_dir, source, version);
    if binary_path.exists() {
        if let Some(lock_entry) = lock_file.find(source, version) {
            let actual_hash =
                sha256_file(&binary_path).map_err(|e| format!("Failed to hash binary: {e}"))?;
            if actual_hash != lock_entry.sha256 {
                return Err(format!(
                    "SHA256 mismatch for provider '{}' ({}@{}). Expected: {}, got: {}. Re-run `carina init` to re-download.",
                    name, source, version, lock_entry.sha256, actual_hash
                ));
            }
        }
        return Ok(binary_path);
    }

    // 3. Check global plugin cache for WASM.
    if let Some(global_wasm) = global_cache_path_wasm(source, version)
        && global_wasm.exists()
    {
        // Copy from global cache to local project
        if let Some(parent) = wasm_path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        fs::hard_link(&global_wasm, &wasm_path)
            .or_else(|_| fs::copy(&global_wasm, &wasm_path).map(|_| ()))
            .map_err(|e| format!("Failed to link/copy from global cache: {e}"))?;
        let hash =
            sha256_file(&wasm_path).map_err(|e| format!("Failed to hash WASM binary: {e}"))?;
        lock_file.upsert(LockEntry {
            name: name.to_string(),
            source: source.to_string(),
            version: version.to_string(),
            constraint: None,
            revision: None,
            resolved_sha: None,
            sha256: hash,
        });
        eprintln!(
            "Installed WASM provider '{}' from global cache ({}@{})",
            name, source, version
        );
        return Ok(wasm_path);
    }

    // 4. Try downloading WASM first (platform-independent).
    let wasm_url = download_url_wasm(source, version)?;
    eprintln!("Downloading WASM provider '{}' from {}", name, wasm_url);
    match download_to_file(&wasm_url, &wasm_path) {
        Ok(()) => {
            let hash =
                sha256_file(&wasm_path).map_err(|e| format!("Failed to hash WASM binary: {e}"))?;
            lock_file.upsert(LockEntry {
                name: name.to_string(),
                source: source.to_string(),
                version: version.to_string(),
                constraint: None,
                revision: None,
                resolved_sha: None,
                sha256: hash,
            });
            // Save to global cache
            if let Some(global_wasm) = global_cache_path_wasm(source, version) {
                if let Some(parent) = global_wasm.parent() {
                    let _ = fs::create_dir_all(parent);
                }
                let _ = fs::hard_link(&wasm_path, &global_wasm)
                    .or_else(|_| fs::copy(&wasm_path, &global_wasm).map(|_| ()));
            }
            eprintln!(
                "Installed WASM provider '{}' ({}@{})",
                name, source, version
            );
            return Ok(wasm_path);
        }
        Err(e) => {
            eprintln!(
                "WASM provider not available ({}), falling back to native binary: {}",
                wasm_url, e
            );
            // Clean up any partial download.
            let _ = fs::remove_file(&wasm_path);
        }
    }

    // 4. Fall back to downloading the native binary.
    let target = detect_target()?;
    let url = download_url(source, version, &target)?;

    eprintln!("Downloading provider '{}' from {}", name, url);

    let tmp_archive = base_dir
        .join(".carina")
        .join("providers")
        .join("tmp_download.tar.gz");

    download_to_file(&url, &tmp_archive)?;

    let dest_dir = binary_path.parent().unwrap();
    let extracted = extract_tar_gz(&tmp_archive, dest_dir)?;

    let _ = fs::remove_file(&tmp_archive);

    if extracted != binary_path {
        fs::rename(&extracted, &binary_path).map_err(|e| {
            format!(
                "Failed to rename {} to {}: {e}",
                extracted.display(),
                binary_path.display()
            )
        })?;
    }

    let hash = sha256_file(&binary_path).map_err(|e| format!("Failed to hash binary: {e}"))?;

    lock_file.upsert(LockEntry {
        name: name.to_string(),
        source: source.to_string(),
        version: version.to_string(),
        constraint: None,
        revision: None,
        resolved_sha: None,
        sha256: hash,
    });

    eprintln!("Installed provider '{}' ({}@{})", name, source, version);

    Ok(binary_path)
}

/// Resolve a single provider config with lock file management.
///
/// Handles version validation, lock file load/save, and delegation to `resolve_provider`.
pub fn resolve_single_config(base_dir: &Path, config: &ProviderConfig) -> Result<PathBuf, String> {
    let source = config
        .source
        .as_deref()
        .ok_or_else(|| format!("Provider '{}' has no source", config.name))?;

    let lock_path = base_dir.join("carina-providers.lock");
    let mut lock_file = LockFile::load(&lock_path)?.unwrap_or_default();

    let binary_path = if let Some(revision) = &config.revision {
        let (path, _sha) = crate::revision_resolver::resolve_provider_by_revision(
            base_dir,
            source,
            revision,
            &config.name,
            &mut lock_file,
            false,
        )?;
        path
    } else {
        let version = resolve_version(source, config, &lock_file, false)?;
        let path = resolve_provider(base_dir, source, &version, &config.name, &mut lock_file)?;

        if let Some(entry) = lock_file.provider.iter_mut().find(|e| e.source == source) {
            entry.constraint = config.version.as_ref().map(|c| c.raw.clone());
        }
        path
    };

    lock_file
        .save(&lock_path)
        .map_err(|e| format!("Failed to save carina-providers.lock: {e}"))?;

    Ok(binary_path)
}

/// Find an already-installed provider without downloading.
///
/// Checks local project cache, global plugin cache, and lock file entries.
/// Returns the path to the WASM binary if found, or an error suggesting
/// `carina init` if not installed.
///
/// This is used by the LSP to avoid filesystem side effects from the editor.
pub fn find_installed_provider(
    base_dir: &Path,
    config: &ProviderConfig,
) -> Result<PathBuf, String> {
    let source = config
        .source
        .as_deref()
        .ok_or_else(|| format!("Provider '{}' has no source", config.name))?;

    // For file:// sources, look in .carina/providers/file/
    if let Some(file_path) = source.strip_prefix("file://") {
        let src = std::path::Path::new(file_path);
        let file_name = src
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("provider");
        let dest = base_dir
            .join(".carina")
            .join("providers")
            .join("file")
            .join(file_name)
            .join(
                src.file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("provider.wasm"),
            );
        if dest.exists() {
            return Ok(dest);
        }
        return Err(format!(
            "not installed. Run `carina init` in {}",
            base_dir.display()
        ));
    }

    let lock_path = base_dir.join("carina-providers.lock");
    let lock_file = LockFile::load(&lock_path)?.unwrap_or_default();

    // Only the project-local `.carina/` counts. The global plugin cache is an
    // install-time optimization consulted by `carina init`; treating it as a
    // runtime source lets validate/plan/apply silently succeed when a prior
    // project already pulled this provider and the current project has no
    // local install yet (issue #2018).
    if let Some(revision) = &config.revision {
        if let Some(lock_entry) = lock_file.find_by_source(source)
            && let Some(ref sha) = lock_entry.resolved_sha
        {
            let wasm_path = crate::revision_resolver::cache_path_revision(base_dir, source, sha);
            if wasm_path.exists() {
                return Ok(wasm_path);
            }
        }
        return Err(format!(
            "not installed. Run `carina init` in {} to install (revision: {})",
            base_dir.display(),
            revision
        ));
    }

    if let Some(lock_entry) = lock_file.find_by_source(source) {
        let version = &lock_entry.version;
        let wasm_path = cache_path_wasm(base_dir, source, version);
        if wasm_path.exists() {
            return Ok(wasm_path);
        }
        let binary_path = cache_path(base_dir, source, version);
        if binary_path.exists() {
            return Ok(binary_path);
        }
    }

    Err(format!(
        "not installed. Run `carina init` in {}",
        base_dir.display()
    ))
}

/// Returns true if the given path points to a WASM provider binary.
pub fn is_wasm_provider(path: &Path) -> bool {
    path.extension().is_some_and(|ext| ext == "wasm")
}

/// Decide whether the locked version can be reused for this version-mode config.
///
/// Returns `None` when the lock entry is missing, belongs to revision mode
/// (its `version` is filler, not a real tag), or fails the configured
/// constraint. The caller must then fetch a real tag. Splitting this out keeps
/// the decision testable and keeps `resolve_version` from ever cloning an
/// empty version string into a URL (issue #2028).
fn try_reuse_locked_version(
    source: &str,
    config: &ProviderConfig,
    lock_file: &LockFile,
) -> Option<String> {
    let entry = lock_file.find_by_source(source)?;

    // Revision-mode entries legitimately store `version = ""` as filler.
    // They carry no usable tag, so version-mode resolution must skip them.
    if entry.version.trim().is_empty() || entry.revision.is_some() {
        return None;
    }

    match &config.version {
        Some(constraint) if constraint.matches(&entry.version).unwrap_or(false) => {
            Some(entry.version.clone())
        }
        None => Some(entry.version.clone()),
        _ => None,
    }
}

/// Resolve the exact version to use for a provider.
fn resolve_version(
    source: &str,
    config: &ProviderConfig,
    lock_file: &LockFile,
    upgrade: bool,
) -> Result<String, String> {
    if !upgrade && let Some(version) = try_reuse_locked_version(source, config, lock_file) {
        return Ok(version);
    }

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
            let tag = crate::version_resolver::fetch_latest_tag(source)?;
            let version = tag.strip_prefix('v').unwrap_or(&tag);
            Ok(version.to_string())
        }
    }
}

/// Resolve all providers that need GitHub source resolution.
pub fn resolve_all(
    base_dir: &Path,
    providers: &[ProviderConfig],
    upgrade: bool,
) -> Result<HashMap<String, PathBuf>, String> {
    let lock_path = base_dir.join("carina-providers.lock");
    let mut lock_file = LockFile::load(&lock_path)?.unwrap_or_default();
    let mut resolved = HashMap::new();

    for config in providers {
        let source = match &config.source {
            Some(s) => s.as_str(),
            _ => continue,
        };

        // Handle file:// sources: copy into .carina/providers/
        if let Some(file_path) = source.strip_prefix("file://") {
            let src_path = PathBuf::from(file_path);
            if !src_path.exists() {
                return Err(format!(
                    "Provider '{}': file source not found: {}",
                    config.name, file_path
                ));
            }
            let file_name = src_path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("provider");
            let dest = base_dir
                .join(".carina")
                .join("providers")
                .join("file")
                .join(file_name)
                .join(
                    src_path
                        .file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or("provider.wasm"),
                );
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent)
                    .map_err(|e| format!("Failed to create provider directory: {e}"))?;
            }
            // Remove existing file before hard-linking (hard_link fails if dest exists)
            let _ = fs::remove_file(&dest);
            fs::hard_link(&src_path, &dest)
                .map_err(|e| format!("Failed to link file:// provider: {e}"))?;
            let sha = sha256_file(&dest)
                .map_err(|e| format!("Failed to compute SHA256 for file:// provider: {e}"))?;

            // Update or add lock entry
            if let Some(entry) = lock_file.provider.iter_mut().find(|e| e.source == source) {
                entry.sha256 = sha;
            } else {
                lock_file.provider.push(LockEntry {
                    name: config.name.clone(),
                    source: source.to_string(),
                    version: "file".to_string(),
                    constraint: None,
                    revision: None,
                    resolved_sha: None,
                    sha256: sha,
                });
            }

            resolved.insert(config.name.clone(), dest);
            continue;
        }

        let binary_path = if let Some(revision) = &config.revision {
            let (path, _sha) = crate::revision_resolver::resolve_provider_by_revision(
                base_dir,
                source,
                revision,
                &config.name,
                &mut lock_file,
                upgrade,
            )?;
            path
        } else {
            let version = resolve_version(source, config, &lock_file, upgrade)?;
            let path = resolve_provider(base_dir, source, &version, &config.name, &mut lock_file)?;

            if let Some(entry) = lock_file.provider.iter_mut().find(|e| e.source == source) {
                entry.constraint = config.version.as_ref().map(|c| c.raw.clone());
            }
            path
        };

        resolved.insert(config.name.clone(), binary_path);
    }

    if !resolved.is_empty() {
        lock_file
            .save(&lock_path)
            .map_err(|e| format!("Failed to save carina-providers.lock: {e}"))?;
    }

    Ok(resolved)
}

/// Validate that locked provider versions still satisfy the configured constraints.
///
/// Called before plan/apply to catch cases where the lock file and constraints have
/// drifted out of sync (e.g., the user tightened a constraint after last `carina init`).
pub fn validate_lock_constraints(
    base_dir: &Path,
    providers: &[ProviderConfig],
) -> Result<(), String> {
    let lock_path = base_dir.join("carina-providers.lock");
    let lock_file = match LockFile::load(&lock_path)? {
        Some(lf) => lf,
        None => return Ok(()),
    };

    for config in providers {
        // Skip revision-based providers — they don't use semver constraints
        if config.revision.is_some() {
            continue;
        }

        let source = match &config.source {
            Some(s) if !s.starts_with("file://") => s.as_str(),
            _ => continue,
        };

        let constraint = match &config.version {
            Some(c) => c,
            None => continue,
        };

        if let Some(lock_entry) = lock_file.find_by_source(source)
            && !constraint.matches(&lock_entry.version).unwrap_or(false)
        {
            return Err(format!(
                "Provider '{}' locked at version {}, but constraint '{}' requires a different version.\nRun `carina init --upgrade` to resolve.",
                config.name, lock_entry.version, constraint.raw
            ));
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn test_detect_target() {
        let target = detect_target().unwrap();
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
    fn test_download_url_wasm() {
        let url = download_url_wasm("github.com/carina-rs/carina-provider-awscc", "0.1.0").unwrap();
        assert_eq!(
            url,
            "https://github.com/carina-rs/carina-provider-awscc/releases/download/v0.1.0/carina-provider-awscc-v0.1.0.wasm"
        );
    }

    #[test]
    fn test_download_url_wasm_invalid_source() {
        let result = download_url_wasm("invalid-source", "0.1.0");
        assert!(result.is_err());
    }

    #[test]
    fn test_download_url_invalid_source() {
        let result = download_url("invalid-source", "0.1.0", "x86_64-unknown-linux-gnu");
        assert!(result.is_err());
    }

    #[test]
    fn test_cache_path() {
        let base = Path::new("/tmp/project");
        let path = cache_path(base, "github.com/carina-rs/carina-provider-awscc", "0.1.0");
        assert_eq!(
            path,
            PathBuf::from(
                "/tmp/project/.carina/providers/github.com/carina-rs/carina-provider-awscc/0.1.0/carina-provider-awscc"
            )
        );
    }

    #[test]
    fn test_cache_path_wasm() {
        let base = Path::new("/tmp/project");
        let path = cache_path_wasm(base, "github.com/carina-rs/carina-provider-awscc", "0.1.0");
        assert_eq!(
            path,
            PathBuf::from(
                "/tmp/project/.carina/providers/github.com/carina-rs/carina-provider-awscc/0.1.0/carina-provider-awscc.wasm"
            )
        );
    }

    #[test]
    fn test_resolve_prefers_wasm_cache() {
        use std::io::Write;

        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        let source = "github.com/carina-rs/carina-provider-awscc";
        let version = "0.1.0";

        // Create a fake WASM file in the cache.
        let wasm_path = cache_path_wasm(base, source, version);
        fs::create_dir_all(wasm_path.parent().unwrap()).unwrap();
        let mut f = fs::File::create(&wasm_path).unwrap();
        f.write_all(b"fake wasm content").unwrap();

        // Also create a fake native binary (should NOT be preferred).
        let native_path = cache_path(base, source, version);
        let mut f2 = fs::File::create(&native_path).unwrap();
        f2.write_all(b"fake native binary").unwrap();

        let mut lock_file = LockFile::default();
        let result = resolve_provider(base, source, version, "awscc", &mut lock_file).unwrap();

        assert_eq!(
            result, wasm_path,
            "WASM cache should be preferred over native binary"
        );
    }

    #[test]
    fn test_lock_file_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("carina-providers.lock");

        let mut lock = LockFile::default();
        lock.upsert(LockEntry {
            name: "awscc".into(),
            source: "github.com/carina-rs/carina-provider-awscc".into(),
            version: "0.1.0".into(),
            constraint: None,
            revision: None,
            resolved_sha: None,
            sha256: "abc123".into(),
        });

        lock.save(&lock_path).unwrap();
        let loaded = LockFile::load(&lock_path).unwrap().unwrap();

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
            constraint: None,
            revision: None,
            resolved_sha: None,
            sha256: "old_hash".into(),
        });
        lock.upsert(LockEntry {
            name: "awscc".into(),
            source: "github.com/carina-rs/carina-provider-awscc".into(),
            version: "0.2.0".into(),
            constraint: None,
            revision: None,
            resolved_sha: None,
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
        assert_eq!(
            hash,
            "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
        );
    }

    #[test]
    fn lock_entry_with_constraint_roundtrip() {
        let lock = LockFile {
            provider: vec![LockEntry {
                name: "aws".to_string(),
                source: "github.com/carina-rs/carina-provider-aws".to_string(),
                version: "0.5.2".to_string(),
                constraint: Some("~0.5.0".to_string()),
                revision: None,
                resolved_sha: None,
                sha256: "abc123".to_string(),
            }],
        };
        let toml_str = toml::to_string_pretty(&lock).unwrap();
        let loaded: LockFile = toml::from_str(&toml_str).unwrap();
        assert_eq!(loaded.provider[0].constraint.as_deref(), Some("~0.5.0"));
    }

    /// An entry with `version = ""` and neither `revision` nor `resolved_sha`
    /// cannot be used by either the version-mode or revision-mode code path.
    /// Loading it as "treat as empty" previously masked the real problem
    /// (#2028) — the resolver would fall through to version-mode, find the
    /// entry by source, and splice `version = ""` into the release URL.
    #[test]
    fn load_rejects_entry_with_neither_version_nor_revision() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("carina-providers.lock");
        fs::write(
            &lock_path,
            r#"
[[provider]]
name = "awscc"
source = "github.com/carina-rs/carina-provider-awscc"
version = ""
sha256 = "effbed"
"#,
        )
        .unwrap();

        let err = LockFile::load(&lock_path)
            .expect_err("entry with no version and no revision must be rejected");
        assert!(err.contains("awscc"), "error should name provider: {err}");
        assert!(
            err.contains("malformed") || err.contains("inconsistent"),
            "error should describe the problem: {err}"
        );
        assert!(
            err.contains("carina init"),
            "error should point at the recovery command: {err}"
        );
    }

    /// Half-populated version-mode entries (version set but revision or
    /// resolved_sha also set) are an impossible combination too.
    #[test]
    fn load_rejects_version_mode_with_resolved_sha() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("carina-providers.lock");
        fs::write(
            &lock_path,
            r#"
[[provider]]
name = "awscc"
source = "github.com/carina-rs/carina-provider-awscc"
version = "0.5.2"
resolved_sha = "deadbeef"
sha256 = "abc123"
"#,
        )
        .unwrap();

        let err = LockFile::load(&lock_path)
            .expect_err("version-mode entry with a resolved_sha must be rejected");
        assert!(err.contains("awscc"), "error should name provider: {err}");
    }

    /// Revision mode without `resolved_sha` cannot be used to locate a
    /// cached binary, so it's also malformed.
    #[test]
    fn load_rejects_revision_without_resolved_sha() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("carina-providers.lock");
        fs::write(
            &lock_path,
            r#"
[[provider]]
name = "awscc"
source = "github.com/carina-rs/carina-provider-awscc"
version = ""
revision = "main"
sha256 = "effbed"
"#,
        )
        .unwrap();

        let err =
            LockFile::load(&lock_path).expect_err("revision without resolved_sha must be rejected");
        assert!(err.contains("awscc"), "error should name provider: {err}");
    }

    /// Happy path: a well-formed version-mode entry loads fine.
    #[test]
    fn load_accepts_version_mode_entry() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("carina-providers.lock");
        fs::write(
            &lock_path,
            r#"
[[provider]]
name = "awscc"
source = "github.com/carina-rs/carina-provider-awscc"
version = "0.5.2"
sha256 = "abc123"
"#,
        )
        .unwrap();

        let lock = LockFile::load(&lock_path)
            .expect("version-mode entry must load")
            .expect("file exists so loader must return Some");
        assert_eq!(lock.provider.len(), 1);
        assert_eq!(lock.provider[0].version, "0.5.2");
    }

    /// Happy path: a well-formed revision-mode entry loads fine. Revision-mode
    /// entries legitimately write `version = ""` as filler; they're distinguished
    /// from malformed entries by also carrying `revision` and `resolved_sha`.
    #[test]
    fn load_accepts_revision_mode_entry() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("carina-providers.lock");
        fs::write(
            &lock_path,
            r#"
[[provider]]
name = "awscc"
source = "github.com/carina-rs/carina-provider-awscc"
version = ""
revision = "main"
resolved_sha = "81b6910fb34e84784daac2a02c915e821b2da570"
sha256 = "abc123"
"#,
        )
        .unwrap();

        let lock = LockFile::load(&lock_path)
            .expect("revision-mode entry must load")
            .expect("file exists so loader must return Some");
        assert_eq!(lock.provider.len(), 1);
        assert_eq!(lock.provider[0].revision.as_deref(), Some("main"));
    }

    /// The original #2028 repro: a lock entry written by a prior revision-mode
    /// `init` (so `version = ""`, `revision = "main"`, `resolved_sha = Some(...)`)
    /// is still present after the user removes `revision` from their `.crn`.
    /// `resolve_version` used to happily return `lock_entry.version.clone()`
    /// (an empty string), which then got spliced into `.../releases/download/v/...`.
    ///
    /// A revision-mode lock entry carries no usable version, so version-mode
    /// resolution must ignore it and go fetch a real tag instead.
    #[test]
    fn resolve_version_ignores_empty_version_from_revision_mode_entry() {
        let mut lock = LockFile::default();
        lock.upsert(LockEntry {
            name: "awscc".into(),
            source: "github.com/carina-rs/carina-provider-awscc".into(),
            version: String::new(),
            constraint: None,
            revision: Some("main".into()),
            resolved_sha: Some("deadbeefcafe".into()),
            sha256: "abc".into(),
        });
        let config = ProviderConfig {
            name: "awscc".into(),
            source: Some("github.com/carina-rs/carina-provider-awscc".into()),
            version: None,
            revision: None,
            attributes: std::collections::HashMap::new(),
            default_tags: std::collections::HashMap::new(),
        };

        // We can't hit the network in a unit test, so just check that the
        // reuse path does not fire. `try_reuse_locked_version` is the pure
        // piece of `resolve_version` — if it returns `None`, the caller
        // proceeds to fetch tags instead of splicing "" into a URL.
        assert!(
            try_reuse_locked_version("github.com/carina-rs/carina-provider-awscc", &config, &lock)
                .is_none(),
            "must not reuse a revision-mode lock entry for a version-mode `.crn`"
        );
    }

    /// Sanity check the happy path: version-mode lock + no constraint → reuse.
    #[test]
    fn resolve_version_reuses_well_formed_lock_entry() {
        let mut lock = LockFile::default();
        lock.upsert(LockEntry {
            name: "awscc".into(),
            source: "github.com/carina-rs/carina-provider-awscc".into(),
            version: "0.5.2".into(),
            constraint: None,
            revision: None,
            resolved_sha: None,
            sha256: "abc".into(),
        });
        let config = ProviderConfig {
            name: "awscc".into(),
            source: Some("github.com/carina-rs/carina-provider-awscc".into()),
            version: None,
            revision: None,
            attributes: std::collections::HashMap::new(),
            default_tags: std::collections::HashMap::new(),
        };

        assert_eq!(
            try_reuse_locked_version("github.com/carina-rs/carina-provider-awscc", &config, &lock),
            Some("0.5.2".to_string())
        );
    }

    /// Defense-in-depth: even if a malformed entry sneaks past the loader,
    /// URL builders must refuse to splice an empty version into the URL
    /// template. Otherwise the user gets a confusing 404 instead of a clear
    /// "your lock file is broken" message.
    #[test]
    fn download_url_rejects_empty_version() {
        let err = download_url(
            "github.com/carina-rs/carina-provider-awscc",
            "",
            "aarch64-apple-darwin",
        )
        .expect_err("download_url must reject empty version");
        assert!(
            err.to_lowercase().contains("version"),
            "error should mention version: {err}"
        );
    }

    #[test]
    fn download_url_wasm_rejects_empty_version() {
        let err = download_url_wasm("github.com/carina-rs/carina-provider-awscc", "")
            .expect_err("download_url_wasm must reject empty version");
        assert!(
            err.to_lowercase().contains("version"),
            "error should mention version: {err}"
        );
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

    #[test]
    fn resolve_all_copies_file_provider() {
        let tmp = tempfile::tempdir().unwrap();
        // Create a fake WASM file
        let wasm_path = tmp.path().join("my-provider.wasm");
        fs::write(&wasm_path, b"fake wasm content").unwrap();

        let source = format!("file://{}", wasm_path.display());
        let providers = vec![ProviderConfig {
            name: "test".to_string(),
            source: Some(source.clone()),
            version: None,
            revision: None,
            attributes: std::collections::HashMap::new(),
            default_tags: std::collections::HashMap::new(),
        }];

        let result = resolve_all(tmp.path(), &providers, false).unwrap();
        assert!(result.contains_key("test"));

        // Verify copied to .carina/providers/file/
        let dest = result.get("test").unwrap();
        assert!(dest.exists());
        assert!(dest.starts_with(tmp.path().join(".carina/providers/file")));

        // Verify lock file created
        let lock = LockFile::load(&tmp.path().join("carina-providers.lock"))
            .unwrap()
            .unwrap();
        let entry = lock.find_by_source(&source).unwrap();
        assert_eq!(entry.version, "file");
        assert!(!entry.sha256.is_empty());
    }

    #[test]
    fn resolve_all_errors_on_missing_file_provider() {
        let tmp = tempfile::tempdir().unwrap();
        let providers = vec![ProviderConfig {
            name: "test".to_string(),
            source: Some("file:///nonexistent/path.wasm".to_string()),
            version: None,
            revision: None,
            attributes: std::collections::HashMap::new(),
            default_tags: std::collections::HashMap::new(),
        }];

        let err = resolve_all(tmp.path(), &providers, false).unwrap_err();
        assert!(err.contains("not found"));
    }

    /// Serialize env-var tests in this module. `CARINA_PLUGIN_CACHE_DIR` is
    /// process-wide state and cargo test runs threads, so tests that touch it
    /// must hold this lock for their whole body.
    fn env_lock() -> &'static std::sync::Mutex<()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
    }

    /// Issue #2018: if the project's local `.carina/` is missing, the binary
    /// on disk that actually matters is absent — even if the shared global
    /// plugin cache still has a copy from a previous project. Falling back to
    /// the global cache makes `validate` claim the project is ready when
    /// re-running `carina init` is the real prerequisite.
    #[test]
    fn find_installed_provider_revision_requires_local_install_not_global_cache() {
        let _guard = env_lock().lock().unwrap_or_else(|p| p.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path();

        let global = tmp.path().join("_global_cache");
        // SAFETY: env_lock() above serializes access with any other test that
        // touches CARINA_PLUGIN_CACHE_DIR in this process.
        unsafe { std::env::set_var("CARINA_PLUGIN_CACHE_DIR", &global) };

        let source = "github.com/carina-rs/carina-provider-awscc";
        let sha = "deadbeefcafe1234567890";

        let lock_path = base.join("carina-providers.lock");
        let mut lock = LockFile::default();
        lock.upsert(LockEntry {
            name: "awscc".into(),
            source: source.into(),
            version: String::new(),
            constraint: None,
            revision: Some("main".into()),
            resolved_sha: Some(sha.into()),
            sha256: "deadbeef".into(),
        });
        lock.save(&lock_path).unwrap();

        // Global cache populated, local `.carina/` deliberately absent.
        let global_wasm =
            crate::revision_resolver::global_cache_path_revision(source, sha).unwrap();
        fs::create_dir_all(global_wasm.parent().unwrap()).unwrap();
        fs::File::create(&global_wasm)
            .unwrap()
            .write_all(b"fake wasm from a prior project")
            .unwrap();

        let config = ProviderConfig {
            name: "awscc".into(),
            source: Some(source.into()),
            version: None,
            revision: Some("main".into()),
            attributes: std::collections::HashMap::new(),
            default_tags: std::collections::HashMap::new(),
        };

        let err = find_installed_provider(base, &config)
            .expect_err("missing local .carina/ must not be masked by a global-cache hit");
        assert!(
            err.contains("carina init"),
            "error should point at `carina init`, got: {err}"
        );

        // SAFETY: still holding env_lock.
        unsafe { std::env::remove_var("CARINA_PLUGIN_CACHE_DIR") };
    }

    /// Companion for the revision case above: same requirement for version-pinned
    /// providers. Lock file + global cache without a local `.carina/` must fail.
    #[test]
    fn find_installed_provider_version_requires_local_install_not_global_cache() {
        let _guard = env_lock().lock().unwrap_or_else(|p| p.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path();

        let global = tmp.path().join("_global_cache");
        // SAFETY: env_lock() serializes.
        unsafe { std::env::set_var("CARINA_PLUGIN_CACHE_DIR", &global) };

        let source = "github.com/carina-rs/carina-provider-awscc";
        let version = "0.1.0";

        let lock_path = base.join("carina-providers.lock");
        let mut lock = LockFile::default();
        lock.upsert(LockEntry {
            name: "awscc".into(),
            source: source.into(),
            version: version.into(),
            constraint: None,
            revision: None,
            resolved_sha: None,
            sha256: "deadbeef".into(),
        });
        lock.save(&lock_path).unwrap();

        let global_wasm = global_cache_path_wasm(source, version).unwrap();
        fs::create_dir_all(global_wasm.parent().unwrap()).unwrap();
        fs::File::create(&global_wasm)
            .unwrap()
            .write_all(b"fake wasm from a prior project")
            .unwrap();

        let config = ProviderConfig {
            name: "awscc".into(),
            source: Some(source.into()),
            version: None,
            revision: None,
            attributes: std::collections::HashMap::new(),
            default_tags: std::collections::HashMap::new(),
        };

        let err = find_installed_provider(base, &config)
            .expect_err("missing local .carina/ must not be masked by a global-cache hit");
        assert!(
            err.contains("carina init"),
            "error should point at `carina init`, got: {err}"
        );

        // SAFETY: still holding env_lock.
        unsafe { std::env::remove_var("CARINA_PLUGIN_CACHE_DIR") };
    }
}
