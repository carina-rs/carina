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
        let content = toml::to_string_pretty(self)
            .map_err(|e| io::Error::other(format!("Failed to serialize lock file: {e}")))?;
        fs::write(path, content)
    }

    pub fn find(&self, source: &str, version: &str) -> Option<&LockEntry> {
        self.provider
            .iter()
            .find(|e| e.source == source && e.version == version)
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

/// Construct the download URL for a provider binary.
pub fn download_url(source: &str, version: &str, target: &str) -> Result<String, String> {
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
    // 1. Check WASM cache first.
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

    // 3. Try downloading WASM first (platform-independent).
    let wasm_url = download_url_wasm(source, version)?;
    println!("Downloading WASM provider '{}' from {}", name, wasm_url);
    match download_to_file(&wasm_url, &wasm_path) {
        Ok(()) => {
            let hash =
                sha256_file(&wasm_path).map_err(|e| format!("Failed to hash WASM binary: {e}"))?;
            lock_file.upsert(LockEntry {
                name: name.to_string(),
                source: source.to_string(),
                version: version.to_string(),
                sha256: hash,
            });
            println!(
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

    println!("Downloading provider '{}' from {}", name, url);

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
        sha256: hash,
    });

    println!("Installed provider '{}' ({}@{})", name, source, version);

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

    let version = config
        .version
        .as_ref()
        .map(|v| v.raw.as_str())
        .ok_or_else(|| {
            format!(
                "Provider '{}' has source but no version. Add: version = \"x.y.z\"",
                config.name
            )
        })?;

    let lock_path = base_dir.join("carina.lock");
    let mut lock_file = LockFile::load(&lock_path).unwrap_or_default();

    let binary_path = resolve_provider(base_dir, source, version, &config.name, &mut lock_file)?;

    lock_file
        .save(&lock_path)
        .map_err(|e| format!("Failed to save carina.lock: {e}"))?;

    Ok(binary_path)
}

/// Returns true if the given path points to a WASM provider binary.
pub fn is_wasm_provider(path: &Path) -> bool {
    path.extension().is_some_and(|ext| ext == "wasm")
}

/// Resolve all providers that need GitHub source resolution.
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
            _ => continue,
        };

        let version = config
            .version
            .as_ref()
            .map(|v| v.raw.as_str())
            .ok_or_else(|| {
                format!(
                    "Provider '{}' has source but no version. Add: version = \"x.y.z\"",
                    config.name
                )
            })?;

        let binary_path =
            resolve_provider(base_dir, source, version, &config.name, &mut lock_file)?;
        resolved.insert(config.name.clone(), binary_path);
    }

    if !resolved.is_empty() {
        lock_file
            .save(&lock_path)
            .map_err(|e| format!("Failed to save carina.lock: {e}"))?;
    }

    Ok(resolved)
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
        assert_eq!(
            hash,
            "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
        );
    }
}
