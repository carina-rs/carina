//! Provider resolution: download, extract, cache, and verify provider binaries.

use std::collections::HashMap;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use carina_core::parser::ProviderConfig;

/// Distinguishes the three shapes a lock entry can take. Encoded as a tagged
/// enum so that invalid field combinations (e.g. `version = ""` *and*
/// `revision = "main"`, the root cause of #2028) can't be constructed at
/// all — no runtime validator, no empty-string filler.
///
/// Serialized with an explicit `mode` discriminator so the on-disk shape is
/// unambiguous:
///
/// ```toml
/// [[provider]]
/// name = "aws"; source = "..."; sha256 = "..."
/// mode = "version"
/// version = "0.5.2"
/// constraint = "~0.5.0"
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "mode", rename_all = "lowercase")]
pub enum LockEntryKind {
    /// Released provider pinned to a semver tag.
    Version {
        version: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        constraint: Option<String>,
    },
    /// Provider built from a git revision (branch/tag/SHA) via CI artifacts.
    Revision {
        revision: String,
        resolved_sha: String,
    },
    /// Local `file://` provider — identified entirely by `source`.
    File,
}

/// A single provider entry in carina-providers.lock.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LockEntry {
    pub name: String,
    pub source: String,
    #[serde(flatten)]
    pub kind: LockEntryKind,
    pub sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registry: Option<RegistryLock>,
}

/// Registry-only pinning metadata. Kept outside [`LockEntryKind`] so the
/// version/revision/file shape remains a closed tagged enum.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RegistryLock {
    pub resolved_hostname: String,
    pub api_base_url: String,
    pub discovery_sha256: String,
    #[serde(default)]
    pub sequence_present: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sequence: Option<u64>,
    #[serde(default)]
    pub valid_until_present: bool,
    #[serde(default)]
    pub signature_present: bool,
    #[serde(default)]
    pub transparency_log_present: bool,
}

/// The full carina-providers.lock file.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LockFile {
    #[serde(default)]
    pub provider: Vec<LockEntry>,
}

impl LockFile {
    fn sources_match(existing: &str, requested: &str) -> bool {
        existing == requested || canonical_lock_source(existing) == canonical_lock_source(requested)
    }

    /// Load `carina-providers.lock`.
    ///
    /// Returns `Ok(None)` when the file is absent (normal first-run case).
    /// Parse errors — including an entry that can't be discriminated into one
    /// of the three [`LockEntryKind`] variants — surface as `Err` rather than
    /// being silently collapsed into a default-empty lock.
    pub fn load(path: &Path) -> Result<Option<Self>, String> {
        let content = match fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("Failed to read {}: {e}", path.display())),
        };
        let lock: Self = toml::from_str(&content).map_err(|e| {
            format!(
                "Failed to parse {}: {e}\nhint: delete {} and re-run `carina init`.",
                path.display(),
                path.display()
            )
        })?;
        Ok(Some(lock))
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        let content = toml::to_string_pretty(self)
            .map_err(|e| io::Error::other(format!("Failed to serialize lock file: {e}")))?;
        fs::write(path, content)
    }

    /// Find a version-mode entry matching `(source, version)`. Revision and
    /// file entries never match — by construction they don't carry a version.
    pub fn find(&self, source: &str, version: &str) -> Option<&LockEntry> {
        self.provider.iter().find(|e| {
            Self::sources_match(&e.source, source)
                && matches!(&e.kind, LockEntryKind::Version { version: v, .. } if v == version)
        })
    }

    pub fn find_by_source(&self, source: &str) -> Option<&LockEntry> {
        self.provider
            .iter()
            .find(|e| Self::sources_match(&e.source, source))
    }

    /// Find a revision-mode entry whose `resolved_sha` matches. Version and
    /// file entries can't have a resolved SHA, so they never match.
    pub fn find_by_source_and_sha(&self, source: &str, sha: &str) -> Option<&LockEntry> {
        self.provider.iter().find(|e| {
            Self::sources_match(&e.source, source)
                && matches!(
                    &e.kind,
                    LockEntryKind::Revision { resolved_sha, .. } if resolved_sha == sha
                )
        })
    }

    pub fn upsert(&mut self, entry: LockEntry) {
        if let Some(existing) = self
            .provider
            .iter_mut()
            .find(|e| Self::sources_match(&e.source, &entry.source))
        {
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

const DEFAULT_REGISTRY_HOST: &str = "registry.carina-rs.dev";
const MAX_SEQUENCE_FAST_FORWARD: u64 = 1_000_000;

#[derive(Debug, Clone, PartialEq, Eq)]
enum ProviderSource {
    GithubDirect,
    Registry(RegistrySource),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RegistrySource {
    source: String,
    hostname: String,
    namespace: String,
    name: String,
}

/// A registry whose hostname and API base were resolved through §1 discovery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedRegistry {
    hostname: String,
    api_base_url: String,
    discovery_sha256: String,
}

#[derive(Debug, Clone)]
struct HttpResponse {
    status: u16,
    body: Vec<u8>,
}

trait RegistryHttp {
    fn get(&self, url: &str) -> Result<HttpResponse, String>;

    fn download_to_file(&self, url: &str, dest: &Path) -> Result<(), String> {
        let response = self.get(url)?;
        if response.status != 200 {
            return Err(format!(
                "Download failed with status {}: {url}",
                response.status
            ));
        }
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("Failed to create directory {}: {e}", parent.display()))?;
        }
        fs::write(dest, response.body)
            .map_err(|e| format!("Failed to write file {}: {e}", dest.display()))?;
        Ok(())
    }
}

struct UreqRegistryHttp;

impl RegistryHttp for UreqRegistryHttp {
    fn get(&self, url: &str) -> Result<HttpResponse, String> {
        let response = match ureq::get(url).call() {
            Ok(response) => response,
            Err(ureq::Error::StatusCode(status)) => {
                return Ok(HttpResponse {
                    status,
                    body: Vec::new(),
                });
            }
            Err(e) => return Err(format!("Failed to fetch {url}: {e}")),
        };
        let status = response.status().into();
        let body = response
            .into_body()
            .read_to_vec()
            .map_err(|e| format!("Failed to read response body from {url}: {e}"))?;
        Ok(HttpResponse { status, body })
    }

    fn download_to_file(&self, url: &str, dest: &Path) -> Result<(), String> {
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
}

#[derive(Debug, Deserialize)]
struct DiscoveryDocument {
    #[serde(rename = "providers.v1")]
    providers_v1: String,
}

#[derive(Debug, Deserialize)]
struct RegistryVersions {
    sequence: Option<u64>,
    valid_until: Option<String>,
    versions: Vec<RegistryVersion>,
}

#[derive(Debug, Deserialize)]
struct RegistryVersion {
    version: String,
}

#[derive(Debug, Deserialize)]
struct RegistryDownload {
    download_url: String,
    shasum: String,
    #[serde(default)]
    signature: Option<serde_json::Value>,
    #[serde(default)]
    transparency_log: Option<serde_json::Value>,
}

fn parse_provider_source(source: &str) -> Result<ProviderSource, String> {
    let parts: Vec<&str> = source.split('/').collect();
    match parts.as_slice() {
        ["github.com", _owner, _repo] => Ok(ProviderSource::GithubDirect),
        [namespace, name] if !namespace.is_empty() && !name.is_empty() => {
            Ok(ProviderSource::Registry(RegistrySource {
                source: format!("{namespace}/{name}"),
                hostname: DEFAULT_REGISTRY_HOST.to_string(),
                namespace: (*namespace).to_string(),
                name: (*name).to_string(),
            }))
        }
        [hostname, namespace, name]
            if !hostname.is_empty() && !namespace.is_empty() && !name.is_empty() =>
        {
            let hostname = hostname.to_ascii_lowercase();
            let source = canonical_registry_source(&hostname, namespace, name);
            Ok(ProviderSource::Registry(RegistrySource {
                source,
                hostname,
                namespace: (*namespace).to_string(),
                name: (*name).to_string(),
            }))
        }
        _ => Err(format!(
            "Invalid source format: {source}. Expected: github.com/{{owner}}/{{repo}} or [hostname/]namespace/name"
        )),
    }
}

fn canonical_registry_source(hostname: &str, namespace: &str, name: &str) -> String {
    if hostname == DEFAULT_REGISTRY_HOST {
        format!("{namespace}/{name}")
    } else {
        format!("{hostname}/{namespace}/{name}")
    }
}

fn canonical_lock_source(source: &str) -> String {
    let parts: Vec<&str> = source.split('/').collect();
    match parts.as_slice() {
        ["github.com", _owner, _repo] => source.to_string(),
        [namespace, name] if !namespace.is_empty() && !name.is_empty() => {
            format!("{namespace}/{name}")
        }
        [hostname, namespace, name]
            if !hostname.is_empty() && !namespace.is_empty() && !name.is_empty() =>
        {
            canonical_registry_source(&hostname.to_ascii_lowercase(), namespace, name)
        }
        _ => source.to_string(),
    }
}

fn sha256_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

fn fetch_json<T: for<'de> Deserialize<'de>, H: RegistryHttp>(
    http: &H,
    url: &str,
) -> Result<(T, Vec<u8>), String> {
    let response = http.get(url)?;
    if response.status != 200 {
        return Err(format!(
            "Registry request failed with status {}: {url}",
            response.status
        ));
    }
    let parsed = serde_json::from_slice(&response.body)
        .map_err(|e| format!("Failed to parse registry JSON from {url}: {e}"))?;
    Ok((parsed, response.body))
}

fn join_registry_url(base: &str, path: &str) -> String {
    format!("{}{}", base.trim_end_matches('/'), path)
}

fn resolve_registry<H: RegistryHttp>(
    source: &RegistrySource,
    http: &H,
) -> Result<ResolvedRegistry, String> {
    let discovery_url = format!("https://{}/.well-known/carina.json", source.hostname);
    let (discovery, body): (DiscoveryDocument, Vec<u8>) = fetch_json(http, &discovery_url)?;
    let api_base_url = resolve_api_base_url(&source.hostname, &discovery.providers_v1)?;
    Ok(ResolvedRegistry {
        hostname: source.hostname.clone(),
        api_base_url,
        discovery_sha256: sha256_bytes(&body),
    })
}

fn resolve_api_base_url(hostname: &str, providers_v1: &str) -> Result<String, String> {
    let origin = format!("https://{hostname}");
    if providers_v1.starts_with("https://") {
        if hostname == DEFAULT_REGISTRY_HOST
            && !providers_v1.starts_with(&format!("{origin}/"))
            && providers_v1 != origin
        {
            return Err(format!(
                "default registry discovery returned cross-origin providers.v1: {providers_v1}"
            ));
        }
        return Ok(ensure_trailing_slash(providers_v1));
    }
    if providers_v1.starts_with("http://") {
        return Err(format!(
            "registry discovery providers.v1 must use HTTPS: {providers_v1}"
        ));
    }
    if providers_v1.starts_with('/') {
        return Ok(ensure_trailing_slash(&format!("{origin}{providers_v1}")));
    }
    Ok(ensure_trailing_slash(&format!("{origin}/{providers_v1}")))
}

fn ensure_trailing_slash(url: &str) -> String {
    if url.ends_with('/') {
        url.to_string()
    } else {
        format!("{url}/")
    }
}

fn fetch_registry_versions<H: RegistryHttp>(
    registry: &ResolvedRegistry,
    source: &RegistrySource,
    lock_file: &LockFile,
    http: &H,
) -> Result<RegistryVersions, String> {
    let url = join_registry_url(
        &registry.api_base_url,
        &format!("/{}/{}/versions", source.namespace, source.name),
    );
    let (versions, _): (RegistryVersions, Vec<u8>) = fetch_json(http, &url)?;
    enforce_registry_freshness(source, lock_file, &versions)?;
    Ok(versions)
}

fn enforce_registry_freshness(
    source: &RegistrySource,
    lock_file: &LockFile,
    versions: &RegistryVersions,
) -> Result<(), String> {
    let locked = lock_file
        .find_by_source(&source.source_key())
        .and_then(|entry| entry.registry.as_ref());
    if locked.is_some_and(|registry| registry.sequence_present) && versions.sequence.is_none() {
        return Err(format!(
            "registry sequence field disappeared for {}/{}",
            source.namespace, source.name
        ));
    }
    if locked.is_some_and(|registry| registry.valid_until_present) && versions.valid_until.is_none()
    {
        return Err(format!(
            "registry valid_until field disappeared for {}/{}",
            source.namespace, source.name
        ));
    }
    if let (Some(registry), Some(sequence)) = (locked, versions.sequence)
        && let Some(previous) = registry.sequence
    {
        if sequence < previous {
            return Err(format!(
                "registry sequence rollback for {}/{}: previous {}, got {}",
                source.namespace, source.name, previous, sequence
            ));
        }
        // TODO(#12): replace this local ceiling with a transparency-log bound
        // once the checksum log is available as the higher-integrity source.
        if sequence.saturating_sub(previous) > MAX_SEQUENCE_FAST_FORWARD {
            return Err(format!(
                "registry sequence fast-forward for {}/{} is too large: previous {}, got {}",
                source.namespace, source.name, previous, sequence
            ));
        }
    }
    if let Some(valid_until) = &versions.valid_until {
        let valid_until = OffsetDateTime::parse(valid_until, &Rfc3339)
            .map_err(|e| format!("Invalid registry valid_until timestamp: {e}"))?;
        if valid_until < OffsetDateTime::now_utc() {
            return Err(format!(
                "registry versions listing valid_until is expired for {}/{}",
                source.namespace, source.name
            ));
        }
    }
    Ok(())
}

impl RegistrySource {
    fn source_key(&self) -> String {
        self.source.clone()
    }
}

fn select_registry_version(
    versions: &[RegistryVersion],
    config: &ProviderConfig,
) -> Result<String, String> {
    match &config.version {
        Some(constraint) => {
            let mut candidates: Vec<Version> = versions
                .iter()
                .filter_map(|entry| Version::parse(&entry.version).ok())
                .filter(|version| constraint.req.matches(version))
                .collect();
            candidates.sort_by(|a, b| b.cmp(a));
            candidates
                .into_iter()
                .next()
                .map(|version| version.to_string())
                .ok_or_else(|| {
                    format!(
                        "No release of '{}' matches constraint '{}'",
                        config.name, constraint.raw
                    )
                })
        }
        None => versions
            .iter()
            .filter_map(|entry| Version::parse(&entry.version).ok())
            .max()
            .map(|version| version.to_string())
            .ok_or_else(|| format!("No versions found for provider '{}'", config.name)),
    }
}

fn fetch_registry_download<H: RegistryHttp>(
    registry: &ResolvedRegistry,
    source: &RegistrySource,
    version: &str,
    http: &H,
) -> Result<RegistryDownload, String> {
    let url = join_registry_url(
        &registry.api_base_url,
        &format!("/{}/{}/{version}/download", source.namespace, source.name),
    );
    let (download, _): (RegistryDownload, Vec<u8>) = fetch_json(http, &url)?;
    Ok(download)
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

/// Validate a cached version-mode binary and ensure the lock file records it.
///
/// When a previous `carina init` left a binary in `.carina/providers/`, the
/// next run must still upsert a matching lock entry before the caller saves
/// the lock. Otherwise an empty in-memory `LockFile` gets written back to
/// disk and stomps the on-disk record (issue #2032).
fn verify_or_record_version_cache(
    binary_path: &Path,
    source: &str,
    version: &str,
    name: &str,
    lock_file: &mut LockFile,
) -> Result<(), String> {
    let actual_hash =
        sha256_file(binary_path).map_err(|e| format!("Failed to hash binary: {e}"))?;
    // Preserve any constraint already recorded; the resolver callers
    // overwrite it afterwards when the `.crn` specifies one.
    let existing_constraint = match lock_file.find(source, version) {
        Some(entry) => {
            if actual_hash != entry.sha256 {
                return Err(format!(
                    "SHA256 mismatch for provider '{}' ({}@{}). Expected: {}, got: {}. Re-run `carina init` to re-download.",
                    name, source, version, entry.sha256, actual_hash
                ));
            }
            match &entry.kind {
                LockEntryKind::Version { constraint, .. } => constraint.clone(),
                _ => None,
            }
        }
        None => None,
    };
    lock_file.upsert(LockEntry {
        name: name.to_string(),
        source: source.to_string(),
        kind: LockEntryKind::Version {
            version: version.to_string(),
            constraint: existing_constraint,
        },
        sha256: actual_hash,
        registry: None,
    });
    Ok(())
}

fn verify_registry_lock_pin(
    lock_file: &LockFile,
    source: &RegistrySource,
    version: &str,
    expected_shasum: &str,
    registry: &ResolvedRegistry,
    signature_present: bool,
    transparency_log_present: bool,
) -> Result<Option<String>, String> {
    let source_key = source.source_key();
    let Some(entry) = lock_file.find_by_source(&source_key) else {
        return Ok(None);
    };
    if matches!(&entry.kind, LockEntryKind::Version { version: locked, .. } if locked == version)
        && entry.sha256 != expected_shasum
    {
        return Err(format!(
            "registry shasum pin mismatch for {}@{}: lock has {}, registry returned {}",
            source_key, version, entry.sha256, expected_shasum
        ));
    }
    let Some(locked_registry) = &entry.registry else {
        return Ok(match &entry.kind {
            LockEntryKind::Version { constraint, .. } => constraint.clone(),
            _ => None,
        });
    };
    if locked_registry.resolved_hostname != registry.hostname {
        return Err(format!(
            "registry hostname pin mismatch for {}: lock has {}, resolved {}",
            source_key, locked_registry.resolved_hostname, registry.hostname
        ));
    }
    if locked_registry.api_base_url != registry.api_base_url {
        return Err(format!("registry API base pin mismatch for {source_key}"));
    }
    if locked_registry.discovery_sha256 != registry.discovery_sha256 {
        return Err(format!(
            "registry discovery document pin mismatch for {source_key}"
        ));
    }
    if locked_registry.signature_present && !signature_present {
        return Err(format!(
            "registry signature field disappeared for {source_key}"
        ));
    }
    if locked_registry.transparency_log_present && !transparency_log_present {
        return Err(format!(
            "registry transparency_log field disappeared for {source_key}"
        ));
    }
    Ok(match &entry.kind {
        LockEntryKind::Version { constraint, .. } => constraint.clone(),
        _ => None,
    })
}

fn resolve_registry_provider_with_http<H: RegistryHttp>(
    base_dir: &Path,
    source: &RegistrySource,
    version: &str,
    name: &str,
    lock_file: &mut LockFile,
    http: &H,
) -> Result<PathBuf, String> {
    let registry = resolve_registry(source, http)?;
    let versions = fetch_registry_versions(&registry, source, lock_file, http)?;
    if !versions
        .versions
        .iter()
        .any(|entry| entry.version == version)
    {
        return Err(format!(
            "Registry provider {} does not contain version {}",
            source.source_key(),
            version
        ));
    }
    let download = fetch_registry_download(&registry, source, version, http)?;
    let signature_present = download.signature.is_some();
    let transparency_log_present = download.transparency_log.is_some();
    let existing_constraint = verify_registry_lock_pin(
        lock_file,
        source,
        version,
        &download.shasum,
        &registry,
        signature_present,
        transparency_log_present,
    )?;

    let wasm_path = cache_path_wasm(base_dir, &source.source_key(), version);
    if wasm_path.exists() {
        let actual_hash =
            sha256_file(&wasm_path).map_err(|e| format!("Failed to hash WASM binary: {e}"))?;
        if actual_hash != download.shasum {
            let _ = fs::remove_file(&wasm_path);
            return Err(format!(
                "SHA256 mismatch for cached registry provider '{}' ({}@{}). Expected registry shasum {}, got {}. Re-run `carina init` to re-download.",
                name,
                source.source_key(),
                version,
                download.shasum,
                actual_hash
            ));
        }
    } else {
        http.download_to_file(&download.download_url, &wasm_path)?;
        let actual_hash =
            sha256_file(&wasm_path).map_err(|e| format!("Failed to hash WASM binary: {e}"))?;
        if actual_hash != download.shasum {
            let _ = fs::remove_file(&wasm_path);
            return Err(format!(
                "SHA256 mismatch for registry provider '{}' ({}@{}). Expected registry shasum {}, got {}.",
                name,
                source.source_key(),
                version,
                download.shasum,
                actual_hash
            ));
        }
    }

    lock_file.upsert(LockEntry {
        name: name.to_string(),
        source: source.source_key(),
        kind: LockEntryKind::Version {
            version: version.to_string(),
            constraint: existing_constraint,
        },
        sha256: download.shasum,
        registry: Some(RegistryLock {
            resolved_hostname: registry.hostname,
            api_base_url: registry.api_base_url,
            discovery_sha256: registry.discovery_sha256,
            sequence_present: versions.sequence.is_some(),
            sequence: versions.sequence,
            valid_until_present: versions.valid_until.is_some(),
            signature_present,
            transparency_log_present,
        }),
    });
    Ok(wasm_path)
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
    resolve_provider_with_http(
        base_dir,
        source,
        version,
        name,
        lock_file,
        &UreqRegistryHttp,
    )
}

fn resolve_provider_with_http<H: RegistryHttp>(
    base_dir: &Path,
    source: &str,
    version: &str,
    name: &str,
    lock_file: &mut LockFile,
    http: &H,
) -> Result<PathBuf, String> {
    if let ProviderSource::Registry(registry_source) = parse_provider_source(source)? {
        return resolve_registry_provider_with_http(
            base_dir,
            &registry_source,
            version,
            name,
            lock_file,
            http,
        );
    }

    // 1. Check local WASM cache first.
    let wasm_path = cache_path_wasm(base_dir, source, version);
    if wasm_path.exists() {
        verify_or_record_version_cache(&wasm_path, source, version, name, lock_file)?;
        return Ok(wasm_path);
    }

    // 2. Check native binary cache.
    let binary_path = cache_path(base_dir, source, version);
    if binary_path.exists() {
        verify_or_record_version_cache(&binary_path, source, version, name, lock_file)?;
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
            kind: LockEntryKind::Version {
                version: version.to_string(),
                constraint: None,
            },
            sha256: hash,
            registry: None,
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
                kind: LockEntryKind::Version {
                    version: version.to_string(),
                    constraint: None,
                },
                sha256: hash,
                registry: None,
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
        kind: LockEntryKind::Version {
            version: version.to_string(),
            constraint: None,
        },
        sha256: hash,
        registry: None,
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
        let version = resolve_version(source, config, &mut lock_file, false)?;
        let path = resolve_provider(base_dir, source, &version, &config.name, &mut lock_file)?;

        if let Some(entry) = lock_file.provider.iter_mut().find(|e| e.source == source)
            && let LockEntryKind::Version { constraint, .. } = &mut entry.kind
        {
            *constraint = config.version.as_ref().map(|c| c.raw.clone());
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
            && let LockEntryKind::Revision { resolved_sha, .. } = &lock_entry.kind
        {
            let wasm_path =
                crate::revision_resolver::cache_path_revision(base_dir, source, resolved_sha);
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

    if let Some(lock_entry) = lock_file.find_by_source(source)
        && let LockEntryKind::Version { version, .. } = &lock_entry.kind
    {
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
/// Returns `None` when the lock entry is missing, is not a version-mode entry,
/// or fails the configured constraint. The pattern match on `LockEntryKind`
/// means revision and file entries can't leak their stored strings into a
/// version-mode URL — the type rules out the #2028 failure mode at the call
/// site, no runtime check needed.
fn try_reuse_locked_version(
    source: &str,
    config: &ProviderConfig,
    lock_file: &LockFile,
) -> Option<String> {
    let entry = lock_file.find_by_source(source)?;
    let LockEntryKind::Version { version, .. } = &entry.kind else {
        return None;
    };

    match &config.version {
        Some(constraint) if constraint.matches(version).unwrap_or(false) => Some(version.clone()),
        None => Some(version.clone()),
        _ => None,
    }
}

/// Resolve the exact version to use for a provider.
fn resolve_version(
    source: &str,
    config: &ProviderConfig,
    lock_file: &mut LockFile,
    upgrade: bool,
) -> Result<String, String> {
    if !upgrade && let Some(version) = try_reuse_locked_version(source, config, lock_file) {
        return Ok(version);
    }

    if let ProviderSource::Registry(registry_source) = parse_provider_source(source)? {
        return resolve_registry_version_with_http(
            &registry_source,
            config,
            lock_file,
            &UreqRegistryHttp,
        );
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

fn resolve_registry_version_with_http<H: RegistryHttp>(
    source: &RegistrySource,
    config: &ProviderConfig,
    lock_file: &mut LockFile,
    http: &H,
) -> Result<String, String> {
    let registry = resolve_registry(source, http)?;
    let versions = fetch_registry_versions(&registry, source, lock_file, http)?;
    select_registry_version(&versions.versions, config)
}

/// How strictly `resolve_all` treats a pre-existing lock file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockMode {
    /// Default for `carina init`: error on mismatch between `.crn` and lock,
    /// but a provider absent from the lock (first-time add) is accepted.
    Normal,
    /// Rebuild the lock from scratch: ignore existing entries and resolve
    /// every provider as if starting fresh. Set by `carina init --upgrade`.
    Upgrade,
    /// Strict CI mode: the lock must match the `.crn` exactly. A provider
    /// present in `.crn` but missing from the lock is an error.
    /// Set by `carina init --locked`. Mirrors Cargo's `--locked`.
    Locked,
}

/// Compare `.crn` provider configs against the lock file and return an
/// error when they disagree. Silent rewrites of the lock on mismatch were
/// defeating the reproducibility contract (issue #2026); every mature tool
/// (Cargo, npm ci, Terraform, Bundler) errors instead.
///
/// Categories detected:
/// - Version constraint that no longer accepts the locked version.
/// - `.crn` switched from version mode to revision mode (or vice versa)
///   since the lock was written.
/// - Same mode but different revision.
/// - (`--locked` only) provider present in `.crn` but missing from the lock.
///
/// Orphan lock entries (present in lock, absent in `.crn`) are intentionally
/// not reported here — they don't block `init` and the normal resolve loop
/// leaves them in place. `--upgrade` is the way to prune.
pub fn check_lock_mismatch(
    providers: &[ProviderConfig],
    lock_file: &LockFile,
    mode: LockMode,
) -> Result<(), String> {
    if mode == LockMode::Upgrade {
        return Ok(());
    }

    for config in providers {
        let source = match &config.source {
            Some(s) if !s.starts_with("file://") => s.as_str(),
            // No source or file:// — either the resolver skips it or the
            // sha256 is refreshed every run, so there's nothing to mismatch.
            _ => continue,
        };

        let lock_entry = match lock_file.find_by_source(source) {
            Some(entry) => entry,
            None => {
                if mode == LockMode::Locked {
                    return Err(format!(
                        "provider '{}' is declared in .crn but missing from carina-providers.lock\n\
                         hint: running with --locked requires the lock to be committed up-to-date;\n\
                               re-run without --locked (or `carina init --upgrade`) to populate it.",
                        config.name
                    ));
                }
                continue;
            }
        };

        match (&config.revision, &config.version, &lock_entry.kind) {
            // .crn revision — lock revision: must match literally.
            (
                Some(crn_rev),
                _,
                LockEntryKind::Revision {
                    revision: locked_rev,
                    ..
                },
            ) => {
                if crn_rev != locked_rev {
                    return Err(mismatch_error(
                        &config.name,
                        &format!("revision = '{locked_rev}'"),
                        &format!("revision = '{crn_rev}'"),
                    ));
                }
            }
            // .crn revision — lock version (mode switched).
            (
                Some(crn_rev),
                _,
                LockEntryKind::Version {
                    version: locked_ver,
                    ..
                },
            ) => {
                return Err(mismatch_error(
                    &config.name,
                    &format!("version  = '{locked_ver}'"),
                    &format!("revision = '{crn_rev}'"),
                ));
            }
            // .crn version constraint — lock version: constraint must still accept it.
            (
                None,
                Some(constraint),
                LockEntryKind::Version {
                    version: locked_ver,
                    ..
                },
            ) => {
                if !constraint.matches(locked_ver).unwrap_or(false) {
                    return Err(mismatch_error(
                        &config.name,
                        &format!("version = '{locked_ver}'"),
                        &format!("constraint = '{}'", constraint.raw),
                    ));
                }
            }
            // .crn version — lock revision (mode switched).
            (
                None,
                Some(constraint),
                LockEntryKind::Revision {
                    revision: locked_rev,
                    ..
                },
            ) => {
                return Err(mismatch_error(
                    &config.name,
                    &format!("revision = '{locked_rev}'"),
                    &format!("version constraint = '{}'", constraint.raw),
                ));
            }
            // No constraint and no revision in .crn: the user didn't pin
            // anything explicitly. That implies version mode (latest tag).
            // Any pre-existing lock entry must also be version mode — a
            // revision-mode entry was written under a `.crn` that had
            // `revision = '...'` and is now gone, which is still a mismatch.
            (None, None, LockEntryKind::Version { .. }) => {}
            (
                None,
                None,
                LockEntryKind::Revision {
                    revision: locked_rev,
                    ..
                },
            ) => {
                return Err(mismatch_error(
                    &config.name,
                    &format!("revision = '{locked_rev}'"),
                    "(no revision, no version constraint — version mode)",
                ));
            }
            // .crn has both revision and version (parser should reject this);
            // treat as accept and let the resolver surface its own error.
            (Some(_), Some(_), _) => {}
            // .crn provider vs a file-mode lock entry: sources shouldn't match,
            // so this arm is effectively unreachable, but bail safely.
            (_, _, LockEntryKind::File) => {}
        }
    }

    Ok(())
}

fn mismatch_error(name: &str, lock_shape: &str, crn_shape: &str) -> String {
    format!(
        "lock file does not match providers.crn\n  \
         provider '{name}':\n    \
         providers.crn:  {crn_shape}\n    \
         lock:           {lock_shape}\n  \
         hint: run `carina init --upgrade` to resolve providers from the current\n        \
         configuration and rewrite carina-providers.lock"
    )
}

/// Resolve all providers that need GitHub source resolution.
pub fn resolve_all(
    base_dir: &Path,
    providers: &[ProviderConfig],
    mode: LockMode,
) -> Result<HashMap<String, PathBuf>, String> {
    let lock_path = base_dir.join("carina-providers.lock");
    let mut lock_file = LockFile::load(&lock_path)?.unwrap_or_default();

    // Fail before touching the filesystem if the lock disagrees with .crn.
    // Rewriting the lock requires `--upgrade`; `--locked` tightens this to
    // require every provider to be present in the lock too.
    check_lock_mismatch(providers, &lock_file, mode)?;

    let upgrade = mode == LockMode::Upgrade;
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
                    kind: LockEntryKind::File,
                    sha256: sha,
                    registry: None,
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
            let version = resolve_version(source, config, &mut lock_file, upgrade)?;
            let path = resolve_provider(base_dir, source, &version, &config.name, &mut lock_file)?;

            if let Some(entry) = lock_file.provider.iter_mut().find(|e| e.source == source)
                && let LockEntryKind::Version { constraint, .. } = &mut entry.kind
            {
                *constraint = config.version.as_ref().map(|c| c.raw.clone());
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
            && let LockEntryKind::Version { version, .. } = &lock_entry.kind
            && !constraint.matches(version).unwrap_or(false)
        {
            return Err(format!(
                "Provider '{}' locked at version {}, but constraint '{}' requires a different version.\nRun `carina init --upgrade` to resolve.",
                config.name, version, constraint.raw
            ));
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use indexmap::IndexMap;
    use std::io::Write;

    fn version_entry(source: &str, version: &str) -> LockEntry {
        LockEntry {
            name: "awscc".into(),
            source: source.into(),
            kind: LockEntryKind::Version {
                version: version.into(),
                constraint: None,
            },
            sha256: "abc".into(),
            registry: None,
        }
    }

    fn revision_entry(source: &str, revision: &str, sha: &str) -> LockEntry {
        LockEntry {
            name: "awscc".into(),
            source: source.into(),
            kind: LockEntryKind::Revision {
                revision: revision.into(),
                resolved_sha: sha.into(),
            },
            sha256: "abc".into(),
            registry: None,
        }
    }

    fn provider_config(source: &str, revision: Option<&str>) -> ProviderConfig {
        ProviderConfig {
            name: "awscc".into(),
            source: Some(source.into()),
            version: None,
            revision: revision.map(|r| r.into()),
            attributes: IndexMap::new(),
            default_tags: IndexMap::new(),
            unresolved_attributes: IndexMap::new(),
            binding: None,
            is_default: true,
        }
    }

    #[test]
    fn detect_target_returns_known_triple() {
        let target = detect_target().unwrap();
        assert!(
            target.contains("apple-darwin") || target.contains("unknown-linux"),
            "Unexpected target: {target}"
        );
    }

    #[test]
    fn download_url_builds_tarball_url() {
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
    fn download_url_wasm_builds_wasm_url() {
        let url = download_url_wasm("github.com/carina-rs/carina-provider-awscc", "0.1.0").unwrap();
        assert_eq!(
            url,
            "https://github.com/carina-rs/carina-provider-awscc/releases/download/v0.1.0/carina-provider-awscc-v0.1.0.wasm"
        );
    }

    #[test]
    fn download_url_rejects_invalid_source() {
        assert!(download_url("invalid-source", "0.1.0", "x86_64-unknown-linux-gnu").is_err());
        assert!(download_url_wasm("invalid-source", "0.1.0").is_err());
    }

    #[test]
    fn cache_path_lays_out_project_local_directory() {
        let base = Path::new("/tmp/project");
        let source = "github.com/carina-rs/carina-provider-awscc";
        assert_eq!(
            cache_path(base, source, "0.1.0"),
            PathBuf::from(
                "/tmp/project/.carina/providers/github.com/carina-rs/carina-provider-awscc/0.1.0/carina-provider-awscc"
            )
        );
        assert_eq!(
            cache_path_wasm(base, source, "0.1.0"),
            PathBuf::from(
                "/tmp/project/.carina/providers/github.com/carina-rs/carina-provider-awscc/0.1.0/carina-provider-awscc.wasm"
            )
        );
    }

    #[test]
    fn resolve_prefers_wasm_cache_over_native_binary() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        let source = "github.com/carina-rs/carina-provider-awscc";
        let version = "0.1.0";

        let wasm_path = cache_path_wasm(base, source, version);
        fs::create_dir_all(wasm_path.parent().unwrap()).unwrap();
        fs::File::create(&wasm_path)
            .unwrap()
            .write_all(b"fake wasm content")
            .unwrap();

        let native_path = cache_path(base, source, version);
        fs::File::create(&native_path)
            .unwrap()
            .write_all(b"fake native binary")
            .unwrap();

        let mut lock_file = LockFile::default();
        let result = resolve_provider(base, source, version, "awscc", &mut lock_file).unwrap();
        assert_eq!(result, wasm_path);
    }

    /// Issue #2032: when `resolve_provider` hits the project-local WASM cache,
    /// it must still upsert a lock entry before returning. Otherwise the caller
    /// writes an empty `LockFile` back to disk on subsequent `carina init` runs
    /// and silently wipes the existing entry.
    #[test]
    fn resolve_upserts_lock_entry_when_wasm_cache_is_hit() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        let source = "github.com/carina-rs/carina-provider-awscc";
        let version = "0.1.0";

        let wasm_path = cache_path_wasm(base, source, version);
        fs::create_dir_all(wasm_path.parent().unwrap()).unwrap();
        fs::File::create(&wasm_path)
            .unwrap()
            .write_all(b"fake wasm content")
            .unwrap();

        let mut lock_file = LockFile::default();
        resolve_provider(base, source, version, "awscc", &mut lock_file).unwrap();

        let entry = lock_file
            .find_by_source(source)
            .expect("cache-hit path must upsert a lock entry");
        match &entry.kind {
            LockEntryKind::Version {
                version: locked, ..
            } => assert_eq!(locked, version),
            other => panic!("expected Version variant, got {other:?}"),
        }
        assert!(!entry.sha256.is_empty(), "entry must record a sha256");
    }

    /// Same guarantee for the native binary cache path.
    #[test]
    fn resolve_upserts_lock_entry_when_native_cache_is_hit() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        let source = "github.com/carina-rs/carina-provider-awscc";
        let version = "0.1.0";

        // Only the native binary exists — no WASM in the cache.
        let native_path = cache_path(base, source, version);
        fs::create_dir_all(native_path.parent().unwrap()).unwrap();
        fs::File::create(&native_path)
            .unwrap()
            .write_all(b"fake native binary")
            .unwrap();

        let mut lock_file = LockFile::default();
        resolve_provider(base, source, version, "awscc", &mut lock_file).unwrap();

        assert!(
            lock_file.find_by_source(source).is_some(),
            "native-cache-hit path must upsert a lock entry"
        );
    }

    /// Round-trip a version-mode entry through TOML. The serialized form carries
    /// an explicit `mode = "version"` discriminator.
    #[test]
    fn version_mode_toml_roundtrip() {
        let source = "github.com/carina-rs/carina-provider-aws";
        let lock = LockFile {
            provider: vec![LockEntry {
                name: "aws".into(),
                source: source.into(),
                kind: LockEntryKind::Version {
                    version: "0.5.2".into(),
                    constraint: Some("~0.5.0".into()),
                },
                sha256: "abc123".into(),
                registry: None,
            }],
        };
        let toml_str = toml::to_string_pretty(&lock).unwrap();
        assert!(
            toml_str.contains("mode = \"version\""),
            "serialized form should tag the variant: {toml_str}"
        );

        let loaded: LockFile = toml::from_str(&toml_str).unwrap();
        assert_eq!(loaded.provider[0].kind, lock.provider[0].kind);
    }

    /// Revision-mode round-trip with the new tag. Note no `version` field.
    #[test]
    fn revision_mode_toml_roundtrip() {
        let lock = LockFile {
            provider: vec![revision_entry(
                "github.com/carina-rs/carina-provider-awscc",
                "main",
                "81b6910fb34e84784daac2a02c915e821b2da570",
            )],
        };
        let toml_str = toml::to_string_pretty(&lock).unwrap();
        assert!(
            toml_str.contains("mode = \"revision\""),
            "serialized form should tag the variant: {toml_str}"
        );
        assert!(
            !toml_str.contains("version ="),
            "revision-mode entry must not serialize a version field: {toml_str}"
        );

        let loaded: LockFile = toml::from_str(&toml_str).unwrap();
        assert_eq!(loaded.provider[0].kind, lock.provider[0].kind);
    }

    #[test]
    fn file_mode_toml_roundtrip() {
        let lock = LockFile {
            provider: vec![LockEntry {
                name: "test".into(),
                source: "file:///tmp/my-provider.wasm".into(),
                kind: LockEntryKind::File,
                sha256: "abc".into(),
                registry: None,
            }],
        };
        let toml_str = toml::to_string_pretty(&lock).unwrap();
        assert!(toml_str.contains("mode = \"file\""), "{toml_str}");

        let loaded: LockFile = toml::from_str(&toml_str).unwrap();
        assert_eq!(loaded.provider[0].kind, LockEntryKind::File);
    }

    /// A lock file with an unknown or missing `mode` tag fails to parse instead
    /// of being silently accepted. That's the type-level replacement for the
    /// runtime validator removed with #2028's fix — there is no more flat shape
    /// the loader has to defend against.
    #[test]
    fn load_rejects_untagged_entry() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("carina-providers.lock");
        fs::write(
            &lock_path,
            r#"
[[provider]]
name = "awscc"
source = "github.com/carina-rs/carina-provider-awscc"
version = "0.5.2"
sha256 = "abc"
"#,
        )
        .unwrap();

        let err = LockFile::load(&lock_path)
            .expect_err("entry without a mode tag must not parse as any variant");
        assert!(
            err.to_lowercase().contains("parse")
                || err.contains("carina init")
                || err.contains("mode"),
            "error should explain the parse failure: {err}"
        );
    }

    #[test]
    fn lock_file_save_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("carina-providers.lock");

        let mut lock = LockFile::default();
        lock.upsert(version_entry(
            "github.com/carina-rs/carina-provider-awscc",
            "0.1.0",
        ));

        lock.save(&lock_path).unwrap();
        let loaded = LockFile::load(&lock_path).unwrap().unwrap();
        assert_eq!(loaded.provider.len(), 1);
        assert_eq!(loaded.provider[0].name, "awscc");
    }

    #[test]
    fn upsert_replaces_existing_entry_by_source() {
        let source = "github.com/carina-rs/carina-provider-awscc";
        let mut lock = LockFile::default();
        lock.upsert(version_entry(source, "0.1.0"));
        lock.upsert(version_entry(source, "0.2.0"));

        assert_eq!(lock.provider.len(), 1);
        match &lock.provider[0].kind {
            LockEntryKind::Version { version, .. } => assert_eq!(version, "0.2.0"),
            other => panic!("expected Version variant, got {other:?}"),
        }
    }

    #[test]
    fn sha256_file_matches_known_digest() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("test.bin");
        fs::File::create(&file_path)
            .unwrap()
            .write_all(b"hello world")
            .unwrap();
        assert_eq!(
            sha256_file(&file_path).unwrap(),
            "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
        );
    }

    /// `find` and `find_by_source_and_sha` now pattern-match on the kind, so a
    /// revision-mode entry never matches a version-mode query and vice versa.
    /// This is the type-level replacement for the runtime guard in #2028.
    #[test]
    fn find_queries_respect_entry_kind() {
        let source = "github.com/carina-rs/carina-provider-awscc";
        let sha = "deadbeefcafe";
        let mut lock = LockFile::default();
        lock.upsert(revision_entry(source, "main", sha));

        // Version-mode query does not match a revision entry.
        assert!(lock.find(source, "0.5.2").is_none());
        // Revision-by-sha query matches.
        assert!(lock.find_by_source_and_sha(source, sha).is_some());

        // Reverse: version-mode entry doesn't answer a revision query.
        let mut lock = LockFile::default();
        lock.upsert(version_entry(source, "0.5.2"));
        assert!(lock.find(source, "0.5.2").is_some());
        assert!(lock.find_by_source_and_sha(source, sha).is_none());
    }

    /// #2028 regression, now enforced by the type: `try_reuse_locked_version`
    /// pattern-matches on `LockEntryKind::Version`, so revision-mode entries
    /// cannot leak their (non-existent) version string into a URL.
    #[test]
    fn try_reuse_skips_revision_mode_entry() {
        let source = "github.com/carina-rs/carina-provider-awscc";
        let mut lock = LockFile::default();
        lock.upsert(revision_entry(source, "main", "deadbeefcafe"));
        let config = provider_config(source, None);

        assert!(
            try_reuse_locked_version(source, &config, &lock).is_none(),
            "revision-mode lock entries must not be reused for version-mode configs"
        );
    }

    #[test]
    fn try_reuse_returns_locked_version_for_version_mode_entry() {
        let source = "github.com/carina-rs/carina-provider-awscc";
        let mut lock = LockFile::default();
        lock.upsert(version_entry(source, "0.5.2"));
        let config = provider_config(source, None);

        assert_eq!(
            try_reuse_locked_version(source, &config, &lock),
            Some("0.5.2".to_string())
        );
    }

    #[test]
    fn resolve_all_copies_file_provider() {
        let tmp = tempfile::tempdir().unwrap();
        let wasm_path = tmp.path().join("my-provider.wasm");
        fs::write(&wasm_path, b"fake wasm content").unwrap();

        let source = format!("file://{}", wasm_path.display());
        let providers = vec![ProviderConfig {
            name: "test".into(),
            source: Some(source.clone()),
            version: None,
            revision: None,
            unresolved_attributes: IndexMap::new(),
            binding: None,
            is_default: true,
            attributes: IndexMap::new(),
            default_tags: IndexMap::new(),
        }];

        let result = resolve_all(tmp.path(), &providers, LockMode::Normal).unwrap();
        let dest = result.get("test").expect("provider should be resolved");
        assert!(dest.exists());
        assert!(dest.starts_with(tmp.path().join(".carina/providers/file")));

        let lock = LockFile::load(&tmp.path().join("carina-providers.lock"))
            .unwrap()
            .unwrap();
        let entry = lock.find_by_source(&source).unwrap();
        assert_eq!(entry.kind, LockEntryKind::File);
        assert!(!entry.sha256.is_empty());
    }

    #[test]
    fn resolve_all_errors_on_missing_file_provider() {
        let tmp = tempfile::tempdir().unwrap();
        let providers = vec![ProviderConfig {
            name: "test".into(),
            source: Some("file:///nonexistent/path.wasm".into()),
            version: None,
            revision: None,
            unresolved_attributes: IndexMap::new(),
            binding: None,
            is_default: true,
            attributes: IndexMap::new(),
            default_tags: IndexMap::new(),
        }];
        let err = resolve_all(tmp.path(), &providers, LockMode::Normal).unwrap_err();
        assert!(err.contains("not found"));
    }

    #[derive(Default)]
    struct FakeRegistryHttp {
        responses: HashMap<String, HttpResponse>,
        downloads: HashMap<String, Vec<u8>>,
        requested: std::sync::Mutex<Vec<String>>,
    }

    impl FakeRegistryHttp {
        fn json(mut self, url: &str, body: &str) -> Self {
            self.responses.insert(
                url.to_string(),
                HttpResponse {
                    status: 200,
                    body: body.as_bytes().to_vec(),
                },
            );
            self
        }

        fn bytes(mut self, url: &str, body: &[u8]) -> Self {
            self.responses.insert(
                url.to_string(),
                HttpResponse {
                    status: 200,
                    body: body.to_vec(),
                },
            );
            self
        }

        fn downloadable_bytes(mut self, url: &str, body: &[u8]) -> Self {
            self.downloads.insert(url.to_string(), body.to_vec());
            self
        }

        fn was_requested(&self, needle: &str) -> bool {
            self.requested
                .lock()
                .unwrap()
                .iter()
                .any(|url| url.contains(needle))
        }
    }

    impl RegistryHttp for FakeRegistryHttp {
        fn get(&self, url: &str) -> Result<HttpResponse, String> {
            self.requested.lock().unwrap().push(url.to_string());
            self.responses
                .get(url)
                .cloned()
                .ok_or_else(|| format!("unexpected test URL: {url}"))
        }

        fn download_to_file(&self, url: &str, dest: &Path) -> Result<(), String> {
            self.requested.lock().unwrap().push(url.to_string());
            let body = self
                .downloads
                .get(url)
                .ok_or_else(|| format!("unexpected test download URL: {url}"))?;
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent)
                    .map_err(|e| format!("Failed to create directory {}: {e}", parent.display()))?;
            }
            fs::write(dest, body)
                .map_err(|e| format!("Failed to write file {}: {e}", dest.display()))?;
            Ok(())
        }
    }

    fn sha256_bytes(bytes: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        format!("{:x}", hasher.finalize())
    }

    fn registry_http(download_body: &[u8], shasum: &str) -> FakeRegistryHttp {
        FakeRegistryHttp::default()
            .json(
                "https://registry.carina-rs.dev/.well-known/carina.json",
                r#"{"providers.v1":"/v1/providers/"}"#,
            )
            .json(
                "https://registry.carina-rs.dev/v1/providers/carina-rs/aws/versions",
                r#"{"sequence":7,"valid_until":"2999-01-01T00:00:00Z","versions":[{"version":"0.5.0","protocols":["1"]}]}"#,
            )
            .json(
                "https://registry.carina-rs.dev/v1/providers/carina-rs/aws/0.5.0/download",
                &format!(
                    r#"{{
                        "protocols":["1"],
                        "filename":"carina-provider-aws-v0.5.0.wasm",
                        "download_url":"https://downloads.example.test/aws.wasm",
                        "shasum":"{shasum}",
                        "shasum_authored_by":"registry",
                        "signature":{{"type":"sigstore-bundle"}}
                    }}"#
                ),
            )
            .bytes("https://downloads.example.test/aws.wasm", download_body)
            .downloadable_bytes("https://downloads.example.test/aws.wasm", download_body)
    }

    #[test]
    fn registry_source_resolves_sections_1_2_3_and_verifies_registry_shasum() {
        let dir = tempfile::tempdir().unwrap();
        let body = b"registry wasm bytes";
        let shasum = sha256_bytes(body);
        let http = registry_http(body, &shasum);
        let mut lock_file = LockFile::default();

        let path = resolve_provider_with_http(
            dir.path(),
            "carina-rs/aws",
            "0.5.0",
            "aws",
            &mut lock_file,
            &http,
        )
        .unwrap();

        assert_eq!(fs::read(&path).unwrap(), body);
        assert!(http.was_requested("/.well-known/carina.json"));
        assert!(http.was_requested("/v1/providers/carina-rs/aws/versions"));
        assert!(http.was_requested("/v1/providers/carina-rs/aws/0.5.0/download"));

        let entry = lock_file.find_by_source("carina-rs/aws").unwrap();
        assert_eq!(entry.sha256, shasum);
        let registry = entry
            .registry
            .as_ref()
            .expect("registry pin must be recorded");
        assert_eq!(registry.resolved_hostname, "registry.carina-rs.dev");
        assert_eq!(
            registry.api_base_url,
            "https://registry.carina-rs.dev/v1/providers/"
        );
        assert!(registry.sequence_present);
        assert_eq!(registry.sequence, Some(7));
        assert!(registry.signature_present);
    }

    #[test]
    fn registry_source_rejects_wasm_when_registry_shasum_mismatches() {
        let dir = tempfile::tempdir().unwrap();
        let http = registry_http(
            b"tampered wasm bytes",
            &sha256_bytes(b"expected wasm bytes"),
        );
        let mut lock_file = LockFile::default();

        let err = resolve_provider_with_http(
            dir.path(),
            "carina-rs/aws",
            "0.5.0",
            "aws",
            &mut lock_file,
            &http,
        )
        .unwrap_err();

        assert!(
            err.to_lowercase().contains("sha256"),
            "mismatch must fail closed before lock/cache use: {err}"
        );
        assert!(
            lock_file.find_by_source("carina-rs/aws").is_none(),
            "mismatched bytes must not be pinned"
        );
    }

    #[test]
    fn registry_source_removes_cached_wasm_when_registry_shasum_mismatches() {
        let dir = tempfile::tempdir().unwrap();
        let body = b"expected wasm bytes";
        let shasum = sha256_bytes(body);
        let http = registry_http(body, &shasum);
        let mut lock_file = LockFile::default();
        let cached_path = cache_path_wasm(dir.path(), "carina-rs/aws", "0.5.0");
        fs::create_dir_all(cached_path.parent().unwrap()).unwrap();
        fs::write(&cached_path, b"stale cached wasm bytes").unwrap();

        let err = resolve_provider_with_http(
            dir.path(),
            "carina-rs/aws",
            "0.5.0",
            "aws",
            &mut lock_file,
            &http,
        )
        .unwrap_err();

        assert!(err.to_lowercase().contains("sha256"), "{err}");
        assert!(
            !cached_path.exists(),
            "bad cached WASM must be removed so the next run can re-download"
        );
        assert!(
            lock_file.find_by_source("carina-rs/aws").is_none(),
            "mismatched cached bytes must not be pinned"
        );
    }

    #[test]
    fn registry_source_rejects_lower_sequence_than_lock_pin() {
        let dir = tempfile::tempdir().unwrap();
        let body = b"registry wasm bytes";
        let shasum = sha256_bytes(body);
        let http = FakeRegistryHttp::default()
            .json(
                "https://registry.carina-rs.dev/.well-known/carina.json",
                r#"{"providers.v1":"/v1/providers/"}"#,
            )
            .json(
                "https://registry.carina-rs.dev/v1/providers/carina-rs/aws/versions",
                r#"{"sequence":6,"valid_until":"2999-01-01T00:00:00Z","versions":[{"version":"0.5.0","protocols":["1"]}]}"#,
            )
            .json(
                "https://registry.carina-rs.dev/v1/providers/carina-rs/aws/0.5.0/download",
                &format!(
                    r#"{{"protocols":["1"],"filename":"aws.wasm","download_url":"https://downloads.example.test/aws.wasm","shasum":"{shasum}","shasum_authored_by":"registry"}}"#
                ),
            )
            .bytes("https://downloads.example.test/aws.wasm", body);
        let mut lock_file = LockFile::default();
        lock_file.upsert(LockEntry {
            name: "aws".into(),
            source: "carina-rs/aws".into(),
            kind: LockEntryKind::Version {
                version: "0.5.0".into(),
                constraint: None,
            },
            sha256: shasum,
            registry: Some(RegistryLock {
                resolved_hostname: "registry.carina-rs.dev".into(),
                api_base_url: "https://registry.carina-rs.dev/v1/providers/".into(),
                discovery_sha256: "old".into(),
                sequence_present: true,
                sequence: Some(7),
                valid_until_present: true,
                signature_present: false,
                transparency_log_present: false,
            }),
        });

        let err = resolve_provider_with_http(
            dir.path(),
            "carina-rs/aws",
            "0.5.0",
            "aws",
            &mut lock_file,
            &http,
        )
        .unwrap_err();
        assert!(err.contains("sequence"), "{err}");
    }

    #[test]
    fn registry_source_rejects_missing_previously_present_sequence() {
        let dir = tempfile::tempdir().unwrap();
        let body = b"registry wasm bytes";
        let shasum = sha256_bytes(body);
        let http = registry_http(body, &shasum).json(
            "https://registry.carina-rs.dev/v1/providers/carina-rs/aws/versions",
            r#"{"valid_until":"2999-01-01T00:00:00Z","versions":[{"version":"0.5.0","protocols":["1"]}]}"#,
        );
        let mut lock_file = LockFile::default();
        lock_file.upsert(LockEntry {
            name: "aws".into(),
            source: "carina-rs/aws".into(),
            kind: LockEntryKind::Version {
                version: "0.5.0".into(),
                constraint: None,
            },
            sha256: shasum,
            registry: Some(RegistryLock {
                resolved_hostname: "registry.carina-rs.dev".into(),
                api_base_url: "https://registry.carina-rs.dev/v1/providers/".into(),
                discovery_sha256: "old".into(),
                sequence_present: true,
                sequence: Some(7),
                valid_until_present: false,
                signature_present: false,
                transparency_log_present: false,
            }),
        });

        let err = resolve_provider_with_http(
            dir.path(),
            "carina-rs/aws",
            "0.5.0",
            "aws",
            &mut lock_file,
            &http,
        )
        .unwrap_err();
        assert!(err.contains("sequence"), "{err}");
    }

    #[test]
    fn registry_source_rejects_missing_previously_present_valid_until() {
        let dir = tempfile::tempdir().unwrap();
        let body = b"registry wasm bytes";
        let shasum = sha256_bytes(body);
        let http = registry_http(body, &shasum).json(
            "https://registry.carina-rs.dev/v1/providers/carina-rs/aws/versions",
            r#"{"sequence":7,"versions":[{"version":"0.5.0","protocols":["1"]}]}"#,
        );
        let mut lock_file = LockFile::default();
        lock_file.upsert(LockEntry {
            name: "aws".into(),
            source: "carina-rs/aws".into(),
            kind: LockEntryKind::Version {
                version: "0.5.0".into(),
                constraint: None,
            },
            sha256: shasum,
            registry: Some(RegistryLock {
                resolved_hostname: "registry.carina-rs.dev".into(),
                api_base_url: "https://registry.carina-rs.dev/v1/providers/".into(),
                discovery_sha256: "old".into(),
                sequence_present: true,
                sequence: Some(7),
                valid_until_present: true,
                signature_present: false,
                transparency_log_present: false,
            }),
        });

        let err = resolve_provider_with_http(
            dir.path(),
            "carina-rs/aws",
            "0.5.0",
            "aws",
            &mut lock_file,
            &http,
        )
        .unwrap_err();
        assert!(err.contains("valid_until"), "{err}");
    }

    #[test]
    fn registry_source_rejects_absurd_sequence_fast_forward() {
        let dir = tempfile::tempdir().unwrap();
        let body = b"registry wasm bytes";
        let shasum = sha256_bytes(body);
        let http = registry_http(body, &shasum).json(
            "https://registry.carina-rs.dev/v1/providers/carina-rs/aws/versions",
            r#"{"sequence":1000000008,"valid_until":"2999-01-01T00:00:00Z","versions":[{"version":"0.5.0","protocols":["1"]}]}"#,
        );
        let mut lock_file = LockFile::default();
        lock_file.upsert(LockEntry {
            name: "aws".into(),
            source: "carina-rs/aws".into(),
            kind: LockEntryKind::Version {
                version: "0.5.0".into(),
                constraint: None,
            },
            sha256: shasum,
            registry: Some(RegistryLock {
                resolved_hostname: "registry.carina-rs.dev".into(),
                api_base_url: "https://registry.carina-rs.dev/v1/providers/".into(),
                discovery_sha256: "old".into(),
                sequence_present: true,
                sequence: Some(7),
                valid_until_present: true,
                signature_present: false,
                transparency_log_present: false,
            }),
        });

        let err = resolve_provider_with_http(
            dir.path(),
            "carina-rs/aws",
            "0.5.0",
            "aws",
            &mut lock_file,
            &http,
        )
        .unwrap_err();
        assert!(err.contains("sequence fast-forward"), "{err}");
    }

    #[test]
    fn registry_source_rejects_missing_previously_present_signature() {
        let dir = tempfile::tempdir().unwrap();
        let body = b"registry wasm bytes";
        let shasum = sha256_bytes(body);
        let http = registry_http(body, &shasum).json(
            "https://registry.carina-rs.dev/v1/providers/carina-rs/aws/0.5.0/download",
            &format!(
                r#"{{"protocols":["1"],"filename":"aws.wasm","download_url":"https://downloads.example.test/aws.wasm","shasum":"{shasum}","shasum_authored_by":"registry"}}"#
            ),
        );
        let mut lock_file = LockFile::default();
        lock_file.upsert(LockEntry {
            name: "aws".into(),
            source: "carina-rs/aws".into(),
            kind: LockEntryKind::Version {
                version: "0.5.0".into(),
                constraint: None,
            },
            sha256: shasum,
            registry: Some(RegistryLock {
                resolved_hostname: "registry.carina-rs.dev".into(),
                api_base_url: "https://registry.carina-rs.dev/v1/providers/".into(),
                discovery_sha256: sha256_bytes(r#"{"providers.v1":"/v1/providers/"}"#.as_bytes()),
                sequence_present: true,
                sequence: Some(7),
                valid_until_present: true,
                signature_present: true,
                transparency_log_present: false,
            }),
        });

        let err = resolve_provider_with_http(
            dir.path(),
            "carina-rs/aws",
            "0.5.0",
            "aws",
            &mut lock_file,
            &http,
        )
        .unwrap_err();
        assert!(err.contains("signature"), "{err}");
    }

    #[test]
    fn registry_source_rejects_signature_downgrade_across_version_change() {
        let dir = tempfile::tempdir().unwrap();
        let old_body = b"old registry wasm bytes";
        let old_shasum = sha256_bytes(old_body);
        let new_body = b"new registry wasm bytes";
        let new_shasum = sha256_bytes(new_body);
        let http = FakeRegistryHttp::default()
            .json(
                "https://registry.carina-rs.dev/.well-known/carina.json",
                r#"{"providers.v1":"/v1/providers/"}"#,
            )
            .json(
                "https://registry.carina-rs.dev/v1/providers/carina-rs/aws/versions",
                r#"{"sequence":7,"valid_until":"2999-01-01T00:00:00Z","versions":[{"version":"0.5.0","protocols":["1"]},{"version":"0.6.0","protocols":["1"]}]}"#,
            )
            .json(
                "https://registry.carina-rs.dev/v1/providers/carina-rs/aws/0.6.0/download",
                &format!(
                    r#"{{"protocols":["1"],"filename":"aws.wasm","download_url":"https://downloads.example.test/aws-0.6.0.wasm","shasum":"{new_shasum}","shasum_authored_by":"registry"}}"#
                ),
            )
            .bytes("https://downloads.example.test/aws-0.6.0.wasm", new_body)
            .downloadable_bytes("https://downloads.example.test/aws-0.6.0.wasm", new_body);
        let mut lock_file = LockFile::default();
        lock_file.upsert(LockEntry {
            name: "aws".into(),
            source: "carina-rs/aws".into(),
            kind: LockEntryKind::Version {
                version: "0.5.0".into(),
                constraint: None,
            },
            sha256: old_shasum,
            registry: Some(RegistryLock {
                resolved_hostname: "registry.carina-rs.dev".into(),
                api_base_url: "https://registry.carina-rs.dev/v1/providers/".into(),
                discovery_sha256: sha256_bytes(r#"{"providers.v1":"/v1/providers/"}"#.as_bytes()),
                sequence_present: true,
                sequence: Some(7),
                valid_until_present: true,
                signature_present: true,
                transparency_log_present: false,
            }),
        });

        let err = resolve_provider_with_http(
            dir.path(),
            "carina-rs/aws",
            "0.6.0",
            "aws",
            &mut lock_file,
            &http,
        )
        .unwrap_err();
        assert!(err.contains("signature"), "{err}");
    }

    #[test]
    fn registry_source_rejects_signature_downgrade_across_default_host_spellings() {
        let dir = tempfile::tempdir().unwrap();
        let old_body = b"old registry wasm bytes";
        let old_shasum = sha256_bytes(old_body);
        let new_body = b"new registry wasm bytes";
        let new_shasum = sha256_bytes(new_body);
        let http = FakeRegistryHttp::default()
            .json(
                "https://registry.carina-rs.dev/.well-known/carina.json",
                r#"{"providers.v1":"/v1/providers/"}"#,
            )
            .json(
                "https://registry.carina-rs.dev/v1/providers/carina-rs/aws/versions",
                r#"{"sequence":7,"valid_until":"2999-01-01T00:00:00Z","versions":[{"version":"0.5.0","protocols":["1"]},{"version":"0.6.0","protocols":["1"]}]}"#,
            )
            .json(
                "https://registry.carina-rs.dev/v1/providers/carina-rs/aws/0.6.0/download",
                &format!(
                    r#"{{"protocols":["1"],"filename":"aws.wasm","download_url":"https://downloads.example.test/aws-0.6.0.wasm","shasum":"{new_shasum}","shasum_authored_by":"registry"}}"#
                ),
            )
            .downloadable_bytes("https://downloads.example.test/aws-0.6.0.wasm", new_body);
        let mut lock_file = LockFile::default();
        lock_file.upsert(LockEntry {
            name: "aws".into(),
            source: "carina-rs/aws".into(),
            kind: LockEntryKind::Version {
                version: "0.5.0".into(),
                constraint: None,
            },
            sha256: old_shasum,
            registry: Some(RegistryLock {
                resolved_hostname: "registry.carina-rs.dev".into(),
                api_base_url: "https://registry.carina-rs.dev/v1/providers/".into(),
                discovery_sha256: sha256_bytes(r#"{"providers.v1":"/v1/providers/"}"#.as_bytes()),
                sequence_present: true,
                sequence: Some(7),
                valid_until_present: true,
                signature_present: true,
                transparency_log_present: false,
            }),
        });

        let err = resolve_provider_with_http(
            dir.path(),
            "registry.carina-rs.dev/carina-rs/aws",
            "0.6.0",
            "aws",
            &mut lock_file,
            &http,
        )
        .unwrap_err();
        assert!(err.contains("signature"), "{err}");
    }

    #[test]
    fn registry_source_migrates_existing_explicit_default_host_lock_entry() {
        let mut lock_file = LockFile::default();
        lock_file.upsert(version_entry(
            "registry.carina-rs.dev/carina-rs/aws",
            "0.5.0",
        ));

        assert!(
            lock_file.find_by_source("carina-rs/aws").is_some(),
            "bare default-host spelling must find old explicit-default lock entries"
        );

        lock_file.upsert(LockEntry {
            name: "aws".into(),
            source: "carina-rs/aws".into(),
            kind: LockEntryKind::Version {
                version: "0.6.0".into(),
                constraint: None,
            },
            sha256: "def".into(),
            registry: None,
        });

        assert_eq!(lock_file.provider.len(), 1);
        assert_eq!(lock_file.provider[0].source, "carina-rs/aws");
    }

    #[test]
    fn registry_source_rejects_missing_previously_present_transparency_log() {
        let dir = tempfile::tempdir().unwrap();
        let body = b"registry wasm bytes";
        let shasum = sha256_bytes(body);
        let http = registry_http(body, &shasum).json(
            "https://registry.carina-rs.dev/v1/providers/carina-rs/aws/0.5.0/download",
            &format!(
                r#"{{"protocols":["1"],"filename":"aws.wasm","download_url":"https://downloads.example.test/aws.wasm","shasum":"{shasum}","shasum_authored_by":"registry","signature":{{"type":"sigstore-bundle"}}}}"#
            ),
        );
        let mut lock_file = LockFile::default();
        lock_file.upsert(LockEntry {
            name: "aws".into(),
            source: "carina-rs/aws".into(),
            kind: LockEntryKind::Version {
                version: "0.5.0".into(),
                constraint: None,
            },
            sha256: shasum,
            registry: Some(RegistryLock {
                resolved_hostname: "registry.carina-rs.dev".into(),
                api_base_url: "https://registry.carina-rs.dev/v1/providers/".into(),
                discovery_sha256: sha256_bytes(r#"{"providers.v1":"/v1/providers/"}"#.as_bytes()),
                sequence_present: true,
                sequence: Some(7),
                valid_until_present: true,
                signature_present: true,
                transparency_log_present: true,
            }),
        });

        let err = resolve_provider_with_http(
            dir.path(),
            "carina-rs/aws",
            "0.5.0",
            "aws",
            &mut lock_file,
            &http,
        )
        .unwrap_err();
        assert!(err.contains("transparency_log"), "{err}");
    }

    #[test]
    fn registry_source_rejects_expired_valid_until() {
        let dir = tempfile::tempdir().unwrap();
        let body = b"registry wasm bytes";
        let shasum = sha256_bytes(body);
        let http = registry_http(body, &shasum).json(
            "https://registry.carina-rs.dev/v1/providers/carina-rs/aws/versions",
            r#"{"sequence":7,"valid_until":"2000-01-01T00:00:00Z","versions":[{"version":"0.5.0","protocols":["1"]}]}"#,
        );
        let mut lock_file = LockFile::default();

        let err = resolve_provider_with_http(
            dir.path(),
            "carina-rs/aws",
            "0.5.0",
            "aws",
            &mut lock_file,
            &http,
        )
        .unwrap_err();
        assert!(err.contains("valid_until"), "{err}");
    }

    #[test]
    fn registry_source_rejects_default_discovery_cross_origin_base() {
        let dir = tempfile::tempdir().unwrap();
        let http = FakeRegistryHttp::default().json(
            "https://registry.carina-rs.dev/.well-known/carina.json",
            r#"{"providers.v1":"https://evil.example.test/v1/providers/"}"#,
        );
        let mut lock_file = LockFile::default();

        let err = resolve_provider_with_http(
            dir.path(),
            "carina-rs/aws",
            "0.5.0",
            "aws",
            &mut lock_file,
            &http,
        )
        .unwrap_err();
        assert!(err.contains("cross-origin"), "{err}");
    }

    #[test]
    fn registry_source_treats_hostname_case_insensitively() {
        let dir = tempfile::tempdir().unwrap();
        let http = FakeRegistryHttp::default().json(
            "https://registry.carina-rs.dev/.well-known/carina.json",
            r#"{"providers.v1":"https://evil.example.test/v1/providers/"}"#,
        );
        let mut lock_file = LockFile::default();

        let err = resolve_provider_with_http(
            dir.path(),
            "Registry.Carina-RS.dev/carina-rs/aws",
            "0.5.0",
            "aws",
            &mut lock_file,
            &http,
        )
        .unwrap_err();
        assert!(err.contains("cross-origin"), "{err}");
    }

    #[test]
    fn registry_source_streams_large_wasm_instead_of_using_capped_get() {
        let dir = tempfile::tempdir().unwrap();
        let body = vec![b'w'; 10 * 1024 * 1024 + 1];
        let shasum = sha256_bytes(&body);
        let http = FakeRegistryHttp::default()
            .json(
                "https://registry.carina-rs.dev/.well-known/carina.json",
                r#"{"providers.v1":"/v1/providers/"}"#,
            )
            .json(
                "https://registry.carina-rs.dev/v1/providers/carina-rs/aws/versions",
                r#"{"sequence":7,"valid_until":"2999-01-01T00:00:00Z","versions":[{"version":"0.5.0","protocols":["1"]}]}"#,
            )
            .json(
                "https://registry.carina-rs.dev/v1/providers/carina-rs/aws/0.5.0/download",
                &format!(
                    r#"{{"protocols":["1"],"filename":"aws.wasm","download_url":"https://downloads.example.test/large-aws.wasm","shasum":"{shasum}","shasum_authored_by":"registry"}}"#
                ),
            )
            .downloadable_bytes("https://downloads.example.test/large-aws.wasm", &body);
        let mut lock_file = LockFile::default();

        let path = resolve_provider_with_http(
            dir.path(),
            "carina-rs/aws",
            "0.5.0",
            "aws",
            &mut lock_file,
            &http,
        )
        .expect("registry WASM download should stream to disk instead of using capped get");

        assert_eq!(sha256_file(&path).unwrap(), shasum);
        assert_eq!(fs::metadata(&path).unwrap().len(), body.len() as u64);
    }

    /// Serialize env-var tests in this module. `CARINA_PLUGIN_CACHE_DIR` is
    /// process-wide state and cargo test runs threads, so tests that touch it
    /// must hold this lock for their whole body.
    fn env_lock() -> &'static std::sync::Mutex<()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
    }

    /// Issue #2018: a lock file + global-cache hit must not mask a missing
    /// local `.carina/`. The project-local directory is the source of truth.
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
        lock.upsert(revision_entry(source, "main", sha));
        lock.save(&lock_path).unwrap();

        let global_wasm =
            crate::revision_resolver::global_cache_path_revision(source, sha).unwrap();
        fs::create_dir_all(global_wasm.parent().unwrap()).unwrap();
        fs::File::create(&global_wasm)
            .unwrap()
            .write_all(b"fake wasm from a prior project")
            .unwrap();

        let config = provider_config(source, Some("main"));
        let err = find_installed_provider(base, &config)
            .expect_err("missing local .carina/ must not be masked by a global-cache hit");
        assert!(err.contains("carina init"), "got: {err}");

        // SAFETY: still holding env_lock.
        unsafe { std::env::remove_var("CARINA_PLUGIN_CACHE_DIR") };
    }

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
        lock.upsert(version_entry(source, version));
        lock.save(&lock_path).unwrap();

        let global_wasm = global_cache_path_wasm(source, version).unwrap();
        fs::create_dir_all(global_wasm.parent().unwrap()).unwrap();
        fs::File::create(&global_wasm)
            .unwrap()
            .write_all(b"fake wasm from a prior project")
            .unwrap();

        let config = provider_config(source, None);
        let err = find_installed_provider(base, &config)
            .expect_err("missing local .carina/ must not be masked by a global-cache hit");
        assert!(err.contains("carina init"), "got: {err}");

        // SAFETY: still holding env_lock.
        unsafe { std::env::remove_var("CARINA_PLUGIN_CACHE_DIR") };
    }

    // --- Issue #2026: lock vs .crn mismatch must error without --upgrade ---

    fn versioned_config(source: &str, constraint: &str) -> ProviderConfig {
        ProviderConfig {
            name: "awscc".into(),
            source: Some(source.into()),
            version: Some(
                carina_core::version_constraint::VersionConstraint::parse(constraint).unwrap(),
            ),
            revision: None,
            unresolved_attributes: IndexMap::new(),
            binding: None,
            is_default: true,
            attributes: IndexMap::new(),
            default_tags: IndexMap::new(),
        }
    }

    const SRC: &str = "github.com/carina-rs/carina-provider-awscc";

    #[test]
    fn check_mismatch_detects_constraint_unsatisfied() {
        let mut lock = LockFile::default();
        lock.upsert(version_entry(SRC, "0.5.2"));
        let cfg = versioned_config(SRC, "~0.6.0");

        let err = check_lock_mismatch(&[cfg], &lock, LockMode::Normal)
            .expect_err("lock version 0.5.2 does not satisfy ~0.6.0 — must error");
        assert!(err.contains("awscc"), "{err}");
        assert!(err.contains("0.5.2"), "{err}");
        assert!(err.contains("~0.6.0"), "{err}");
        assert!(err.contains("--upgrade"), "{err}");
    }

    #[test]
    fn check_mismatch_detects_version_to_revision_switch() {
        let mut lock = LockFile::default();
        lock.upsert(version_entry(SRC, "0.5.2"));
        let cfg = provider_config(SRC, Some("main"));

        let err = check_lock_mismatch(&[cfg], &lock, LockMode::Normal)
            .expect_err(".crn revision vs lock version must error");
        assert!(err.contains("awscc"), "{err}");
        assert!(err.contains("revision"), "{err}");
        assert!(err.contains("version"), "{err}");
        assert!(err.contains("--upgrade"), "{err}");
    }

    #[test]
    fn check_mismatch_detects_revision_to_version_switch() {
        let mut lock = LockFile::default();
        lock.upsert(revision_entry(SRC, "main", "abc123"));
        let cfg = versioned_config(SRC, "~0.5.0");

        let err = check_lock_mismatch(&[cfg], &lock, LockMode::Normal)
            .expect_err(".crn version vs lock revision must error");
        assert!(err.contains("awscc"), "{err}");
        assert!(err.contains("--upgrade"), "{err}");
    }

    #[test]
    fn check_mismatch_detects_revision_change() {
        let mut lock = LockFile::default();
        lock.upsert(revision_entry(SRC, "main", "abc123"));
        let cfg = provider_config(SRC, Some("develop"));

        let err = check_lock_mismatch(&[cfg], &lock, LockMode::Normal)
            .expect_err(".crn revision changed vs lock — must error");
        assert!(err.contains("awscc"), "{err}");
        assert!(err.contains("main"), "{err}");
        assert!(err.contains("develop"), "{err}");
        assert!(err.contains("--upgrade"), "{err}");
    }

    /// Adding a new provider not in the lock is fine in Normal mode — that's
    /// the expected first-time flow.
    #[test]
    fn check_mismatch_allows_new_provider_in_normal_mode() {
        let lock = LockFile::default();
        let cfg = provider_config(SRC, Some("main"));
        assert!(check_lock_mismatch(&[cfg], &lock, LockMode::Normal).is_ok());
    }

    /// In `--locked` mode, a provider missing from the lock is an error (the
    /// lock is supposed to be the full source of truth, matching `cargo --locked`).
    #[test]
    fn check_mismatch_rejects_new_provider_in_locked_mode() {
        let lock = LockFile::default();
        let cfg = provider_config(SRC, Some("main"));

        let err = check_lock_mismatch(&[cfg], &lock, LockMode::Locked)
            .expect_err("--locked must error when a provider is missing from the lock");
        assert!(err.contains("awscc"), "{err}");
        assert!(err.contains("locked"), "{err}");
    }

    /// Happy path: lock matches .crn exactly → no error.
    #[test]
    fn check_mismatch_accepts_matching_version() {
        let mut lock = LockFile::default();
        lock.upsert(version_entry(SRC, "0.5.2"));
        let cfg = versioned_config(SRC, "~0.5.0");

        assert!(check_lock_mismatch(&[cfg], &lock, LockMode::Normal).is_ok());
    }

    #[test]
    fn check_mismatch_accepts_matching_revision() {
        let mut lock = LockFile::default();
        lock.upsert(revision_entry(SRC, "main", "abc"));
        let cfg = provider_config(SRC, Some("main"));

        assert!(check_lock_mismatch(&[cfg], &lock, LockMode::Normal).is_ok());
    }

    /// .crn without a version constraint and lock with a pinned version is OK
    /// (no constraint means "accept whatever is locked").
    #[test]
    fn check_mismatch_accepts_unconstrained_version_config() {
        let mut lock = LockFile::default();
        lock.upsert(version_entry(SRC, "0.5.2"));
        let cfg = provider_config(SRC, None);

        assert!(check_lock_mismatch(&[cfg], &lock, LockMode::Normal).is_ok());
    }

    /// .crn without revision but lock in revision mode is a mismatch — the
    /// user dropped `revision = '...'` from their config and `.crn` now
    /// implies version mode, but the lock still pins a git revision.
    #[test]
    fn check_mismatch_detects_revision_dropped_from_crn() {
        let mut lock = LockFile::default();
        lock.upsert(revision_entry(SRC, "main", "abc123"));
        let cfg = provider_config(SRC, None); // no revision, no version

        let err = check_lock_mismatch(&[cfg], &lock, LockMode::Normal)
            .expect_err(".crn lost its revision but lock still has one — must error");
        assert!(err.contains("awscc"), "{err}");
        assert!(err.contains("main"), "{err}");
        assert!(err.contains("--upgrade"), "{err}");
    }

    /// End-to-end: `resolve_all` in Normal mode with a stale lock errors
    /// *before* doing any network or filesystem work, and leaves the existing
    /// lock file untouched. That's the invariant the whole fix is built on.
    #[test]
    fn resolve_all_errors_on_mismatch_without_touching_lock() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path();
        let lock_path = base.join("carina-providers.lock");

        // Pre-existing lock: revision mode.
        let mut lock = LockFile::default();
        lock.upsert(revision_entry(SRC, "main", "abc123"));
        lock.save(&lock_path).unwrap();
        let before = fs::read_to_string(&lock_path).unwrap();

        // .crn now wants a version — should error, not fall through to a
        // network fetch, and not rewrite the lock.
        let providers = vec![versioned_config(SRC, "~0.5.0")];
        let err = resolve_all(base, &providers, LockMode::Normal)
            .expect_err("mismatched lock must abort resolve_all");
        assert!(err.contains("--upgrade"), "{err}");

        let after = fs::read_to_string(&lock_path).unwrap();
        assert_eq!(before, after, "lock must be untouched on mismatch error");
    }

    /// file:// providers skip the lock-mismatch check — their `sha256` is
    /// refreshed on every `init` by design.
    #[test]
    fn check_mismatch_skips_file_sources() {
        let mut lock = LockFile::default();
        lock.upsert(LockEntry {
            name: "test".into(),
            source: "file:///tmp/provider.wasm".into(),
            kind: LockEntryKind::File,
            sha256: "abc".into(),
            registry: None,
        });
        let cfg = ProviderConfig {
            name: "test".into(),
            source: Some("file:///tmp/provider.wasm".into()),
            version: None,
            revision: None,
            unresolved_attributes: IndexMap::new(),
            binding: None,
            is_default: true,
            attributes: IndexMap::new(),
            default_tags: IndexMap::new(),
        };
        assert!(check_lock_mismatch(&[cfg], &lock, LockMode::Normal).is_ok());
    }
}
