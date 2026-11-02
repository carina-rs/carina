//! Filesystem identity and user-facing path spelling are separate concerns.

#![allow(clippy::disallowed_methods)]

use std::fmt;
use std::io;
use std::io::Write;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Deserializer, Serialize};

/// A filesystem address pinned once, with a separate spelling for diagnostics.
///
/// IO belongs to this value. Its resolved spelling cannot escape this module.
/// Missing destinations retain a resolved parent so creating a state or lock
/// never traverses the original project alias again. Children resolve relative
/// to that pinned parent, including `..` and child symlinks.
///
/// ```compile_fail,E0599
/// use carina_core::paths::ResolvedPath;
/// let path = ResolvedPath::new(".").unwrap();
/// let _: &std::path::Path = path.as_ref();
/// ```
///
/// ```compile_fail,E0599
/// use carina_core::paths::ResolvedPath;
/// let path = ResolvedPath::new(".").unwrap();
/// let _: &std::path::Path = path.as_path();
/// ```
///
/// ```compile_fail,E0308
/// use carina_core::paths::ResolvedPath;
/// let path = ResolvedPath::new(".").unwrap();
/// let _: &std::path::Path = &path;
/// ```
///
/// ```compile_fail,E0614
/// use carina_core::paths::ResolvedPath;
/// let path = ResolvedPath::new(".").unwrap();
/// let _: &std::path::Path = &*path;
/// ```
///
/// ```compile_fail,E0277
/// use carina_core::paths::ResolvedPath;
/// let path = ResolvedPath::new(".").unwrap();
/// let _ = format!("{}", path);
/// ```
///
/// ```compile_fail,E0277
/// use carina_core::paths::ResolvedPath;
/// let path = ResolvedPath::new(".").unwrap();
/// let _ = std::path::PathBuf::from(path);
/// ```
///
/// Mid-operation APIs require an already pinned address.
///
/// ```compile_fail,E0308
/// use carina_core::config_loader::load_configuration;
/// let _ = load_configuration(std::path::Path::new("."));
/// ```
#[derive(Clone, PartialEq, Eq)]
pub struct ResolvedPath {
    resolved: PathBuf,
    display: DisplayPath,
    // Capture the logical address before pinning symlinks, for IO and persistence.
    display_address: PathBuf,
    persisted: PersistedPath,
    sibling_parent: Option<PathBuf>,
}

impl fmt::Debug for ResolvedPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("ResolvedPath").field(&self.display).finish()
    }
}

impl ResolvedPath {
    pub fn new(path: impl AsRef<Path>) -> io::Result<Self> {
        let display = DisplayPath::new(path)?;
        let address = absolute_io_address(&display.0)?;
        let persisted = PersistedPath::new(&address)?;
        Self::from_address(&address, display, address.clone(), persisted)
    }

    fn from_address(
        address: &Path,
        display: DisplayPath,
        display_address: PathBuf,
        persisted: PersistedPath,
    ) -> io::Result<Self> {
        Self::from_address_after_parent_pin(address, display, display_address, persisted, || {})
    }

    fn from_address_after_parent_pin(
        address: &Path,
        display: DisplayPath,
        display_address: PathBuf,
        persisted: PersistedPath,
        after_parent_pin: impl FnOnce(),
    ) -> io::Result<Self> {
        let sibling_parent = address
            .parent()
            .map(|parent| pin_io_path(parent, 0))
            .transpose()?;
        after_parent_pin();
        // Resolve the leaf only through the parent we just pinned. Resolving
        // the complete locator again could follow a retargeted parent alias
        // and pair one tree's state with another tree's advisory lock.
        let resolved = match (&sibling_parent, address.components().next_back()) {
            (Some(parent), Some(leaf)) => pin_io_path(&parent.join(leaf.as_os_str()), 0)?,
            _ => pin_io_path(address, 0)?,
        };
        Ok(Self {
            resolved,
            display,
            display_address,
            persisted,
            sibling_parent,
        })
    }

    pub fn parent(&self) -> Option<Self> {
        let resolved = self.sibling_parent.clone()?;
        Some(Self {
            sibling_parent: resolved.parent().map(Path::to_path_buf),
            resolved,
            display: DisplayPath(self.display.0.parent()?.to_path_buf()),
            display_address: self.display_address.parent()?.to_path_buf(),
            persisted: PersistedPath(self.persisted.0.parent()?),
        })
    }

    /// Resolve a complete diagnostic locator, whose relative spelling is
    /// process-CWD-relative. Locators under this path reuse its pinned tree.
    pub fn join_display(&self, child: &DisplayPath) -> io::Result<Self> {
        let address = if child.0.is_absolute() {
            child.0.clone()
        } else if let Some(relative) = strip_display_prefix(&child.0, &self.display.0) {
            self.display_address.join(relative)
        } else {
            absolute_io_address(&child.0)?
        };
        let persisted = PersistedPath::new(&address)?;
        let io_address = match address
            .strip_prefix(&self.display_address)
            .or_else(|_| address.strip_prefix(&self.persisted.0.0))
        {
            Ok(relative) => self.resolved.join(relative),
            Err(_) => address.clone(),
        };
        Self::from_address(&io_address, child.clone(), address, persisted)
    }

    pub fn display(&self) -> &DisplayPath {
        &self.display
    }

    /// Absolute locator captured without resolving the user's symlink spelling.
    /// Persist this value when the path will be reused from another directory.
    pub fn persisted_path(&self) -> &PersistedPath {
        &self.persisted
    }

    /// Identity of the pinned address, without any further resolution.
    pub fn identity(&self) -> CanonicalPath {
        CanonicalPath(self.resolved.clone())
    }

    /// Resolve a child from this pinned address. Absolute paths produced from
    /// the carried display spelling are mapped back into the same pinned tree.
    pub fn join(&self, child: impl AsRef<Path>) -> io::Result<Self> {
        let child = child.as_ref();
        let relative = if child.is_absolute() {
            child
                .strip_prefix(&self.display_address)
                .or_else(|_| child.strip_prefix(&self.persisted.0.0))
                .unwrap_or(child)
        } else {
            child
        };
        Self::from_address(
            &self.resolved.join(relative),
            self.display.join(child),
            self.display_address.join(child),
            self.persisted.join(child)?,
        )
    }

    pub fn with_extension(&self, extension: impl AsRef<std::ffi::OsStr>) -> io::Result<Self> {
        let name = self
            .display
            .0
            .file_name()
            .ok_or_else(|| io::Error::other("path has no file name"))?;
        self.parent()
            .ok_or_else(|| io::Error::other("path has no parent"))?
            .join(Path::new(name).with_extension(extension))
    }

    pub fn create_dir(&self) -> io::Result<()> {
        std::fs::create_dir(&self.resolved)
    }

    pub fn create_dir_all(&self) -> io::Result<()> {
        std::fs::create_dir_all(&self.resolved)
    }

    /// Open the persistent advisory-lock inode without truncating it.
    pub fn open_lock(&self) -> io::Result<std::fs::File> {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&self.resolved)
    }

    pub fn read(&self) -> io::Result<Vec<u8>> {
        std::fs::read(&self.resolved)
    }

    pub fn read_to_string(&self) -> io::Result<String> {
        std::fs::read_to_string(&self.resolved)
    }

    #[cfg(feature = "async-io")]
    pub async fn read_async(&self) -> io::Result<Vec<u8>> {
        tokio::fs::read(&self.resolved).await
    }

    #[cfg(feature = "async-io")]
    pub async fn read_to_string_async(&self) -> io::Result<String> {
        tokio::fs::read_to_string(&self.resolved).await
    }

    pub fn open_read(&self) -> io::Result<std::fs::File> {
        std::fs::File::open(&self.resolved)
    }

    pub fn metadata(&self) -> io::Result<std::fs::Metadata> {
        std::fs::metadata(&self.resolved)
    }

    /// Inspect the original leaf entry under its pinned parent without following
    /// the leaf symlink. The project alias is never traversed again.
    pub fn symlink_metadata(&self) -> io::Result<std::fs::Metadata> {
        match (&self.sibling_parent, self.display.file_name()) {
            (Some(parent), Some(name)) => std::fs::symlink_metadata(parent.join(name)),
            _ => std::fs::symlink_metadata(&self.resolved),
        }
    }

    pub fn exists(&self) -> bool {
        self.resolved.exists()
    }

    pub fn is_file(&self) -> bool {
        self.resolved.is_file()
    }

    pub fn is_dir(&self) -> bool {
        self.resolved.is_dir()
    }

    pub fn remove(&self) -> io::Result<()> {
        std::fs::remove_file(&self.resolved)
    }

    #[cfg(feature = "async-io")]
    pub async fn remove_async(&self) -> io::Result<()> {
        tokio::fs::remove_file(&self.resolved).await
    }

    /// Exclusive creation, including durable contents and cleanup on failure.
    pub fn create_lock(&self, content: &[u8]) -> io::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&self.resolved)?;
        if let Err(error) = file.write_all(content).and_then(|()| file.sync_all()) {
            let _ = self.remove();
            return Err(error);
        }
        Ok(())
    }

    /// Stage beside the pinned target, preserving an existing file's mode.
    /// Neither the staging name nor the destination can escape this module.
    pub fn stage(&self) -> io::Result<StagedFile> {
        let permissions = match self.metadata() {
            Ok(metadata) => Some(metadata.permissions()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        let name = self
            .resolved
            .file_name()
            .unwrap_or_default()
            .to_string_lossy();
        let temporary = self
            .resolved
            .with_file_name(format!(".{name}.tmp.{}", uuid::Uuid::new_v4()));
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        let stage = StagedFile {
            file,
            temporary,
            destination: self.resolved.clone(),
        };
        if let Some(permissions) = permissions {
            stage.file.set_permissions(permissions)?;
        }
        Ok(stage)
    }

    /// Replace atomically, sync contents and parent, and preserve the file mode.
    pub fn atomic_replace(&self, content: &[u8]) -> io::Result<()> {
        let mut stage = self.stage()?;
        stage.write_all(content)?;
        stage.publish()
    }

    #[cfg(feature = "async-io")]
    pub async fn atomic_replace_async(&self, content: Vec<u8>) -> io::Result<()> {
        let target = self.clone();
        tokio::task::spawn_blocking(move || target.atomic_replace(&content))
            .await
            .map_err(io::Error::other)?
    }

    /// Sorted entries, each pinned under this directory and retaining its
    /// diagnostic spelling. No `DirEntry` or resolved path escapes.
    pub fn entries(&self) -> io::Result<Vec<Self>> {
        let mut names = std::fs::read_dir(&self.resolved)?
            .map(|entry| entry.map(|entry| entry.file_name()))
            .collect::<io::Result<Vec<_>>>()?;
        names.sort();
        names.into_iter().map(|name| self.join(name)).collect()
    }

    pub fn crn_files(&self) -> io::Result<Vec<Self>> {
        let mut names = std::fs::read_dir(&self.resolved)?
            .map(|entry| entry.map(|entry| entry.file_name()))
            .collect::<io::Result<Vec<_>>>()?;
        names.retain(|name| {
            Path::new(name)
                .extension()
                .is_some_and(|extension| extension == "crn")
        });
        names.sort();
        names.into_iter().map(|name| self.join(name)).collect()
    }
}

fn pin_io_path(path: &Path, links: usize) -> io::Result<PathBuf> {
    match std::fs::canonicalize(path) {
        Ok(resolved) => Ok(resolved),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            if links >= 40 {
                return Err(io::Error::other("too many symbolic links in IO path"));
            }
            if std::fs::symlink_metadata(path)
                .is_ok_and(|metadata| metadata.file_type().is_symlink())
            {
                let target = std::fs::read_link(path)?;
                let target = path.parent().unwrap_or(Path::new("/")).join(target);
                return pin_io_path(&target, links + 1);
            }
            let mut components = path.components();
            let component = components.next_back().ok_or(error)?;
            let parent = pin_io_path(components.as_path(), links)?;
            match component {
                Component::ParentDir => Ok(parent.parent().unwrap_or(&parent).to_path_buf()),
                Component::CurDir => Ok(parent),
                component => Ok(parent.join(component.as_os_str())),
            }
        }
        Err(error) => Err(error),
    }
}

/// An opaque, symlink-resolved filesystem identity.
///
/// Keep a separate [`DisplayPath`] for diagnostics. Even `Debug` deliberately
/// hides the identity's spelling.
///
/// ```compile_fail,E0277
/// use carina_core::paths::CanonicalPath;
/// let p = CanonicalPath::new(".").unwrap();
/// let message = format!("{}", p);
/// ```
///
/// ```compile_fail,E0599
/// use carina_core::paths::CanonicalPath;
/// let p = CanonicalPath::new(".").unwrap();
/// let message = p.display();
/// ```
///
/// Relative identities cannot be converted into diagnostic strings either.
///
/// ```compile_fail,E0599
/// use carina_core::paths::CanonicalPath;
/// let p = CanonicalPath::new(".").unwrap();
/// let message: Option<String> = p.relative_identity(&p);
/// ```
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct CanonicalPath(PathBuf);

impl CanonicalPath {
    pub fn ancestors(&self) -> impl Iterator<Item = Self> + '_ {
        self.0.ancestors().map(|path| Self(path.to_path_buf()))
    }

    pub fn new(path: impl AsRef<Path>) -> io::Result<Self> {
        path.as_ref().canonicalize().map(Self)
    }

    /// Produce the portable relative identity used by saved composition calls.
    /// The absolute canonical spelling never leaves this module.
    pub(crate) fn composition_source_relative_to(
        &self,
        root: &Self,
    ) -> Result<
        Option<crate::resource::composition::SourceRootRelativePath>,
        crate::resource::composition::SavedCompositionSourcePathError,
    > {
        relative_path_from_root(&root.0, &self.0)
            .map(crate::resource::composition::SourceRootRelativePath::try_from)
            .transpose()
    }
}

/// An opaque staged file. Publication is confined to its pinned destination.
///
/// ```compile_fail,E0599
/// use carina_core::paths::ResolvedPath;
/// let stage = ResolvedPath::new("state.json").unwrap().stage().unwrap();
/// let _ = stage.path();
/// ```
pub struct StagedFile {
    file: std::fs::File,
    temporary: PathBuf,
    destination: PathBuf,
}

impl StagedFile {
    pub fn metadata(&self) -> io::Result<std::fs::Metadata> {
        self.file.metadata()
    }
    pub fn sync_all(&self) -> io::Result<()> {
        self.file.sync_all()
    }
    pub fn publish(self) -> io::Result<()> {
        self.file.sync_all()?;
        std::fs::rename(&self.temporary, &self.destination)?;
        if let Some(parent) = self.destination.parent() {
            #[cfg(unix)]
            std::fs::File::open(parent)?.sync_all()?;
            // Platforms without portable directory handles retain best-effort synchronization.
            #[cfg(not(unix))]
            if let Ok(directory) = std::fs::File::open(parent) {
                let _ = directory.sync_all();
            }
        }
        Ok(())
    }
}

impl io::Write for StagedFile {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.file.write(buffer)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}
impl io::Read for StagedFile {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.file.read(buffer)
    }
}
impl io::Seek for StagedFile {
    fn seek(&mut self, position: io::SeekFrom) -> io::Result<u64> {
        self.file.seek(position)
    }
}
impl Drop for StagedFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.temporary);
    }
}

impl fmt::Debug for CanonicalPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CanonicalPath(..)")
    }
}

impl<'de> Deserialize<'de> for CanonicalPath {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let path = PathBuf::deserialize(deserializer)?;
        if !path.is_absolute()
            || path
                .components()
                .any(|part| matches!(part, Component::CurDir | Component::ParentDir))
        {
            return Err(serde::de::Error::custom(
                "canonical identity must be an absolute path without dot components",
            ));
        }
        // Restore the saved identity without resolving it against today's filesystem.
        Ok(Self(path))
    }
}

/// A diagnostic path retaining the user's spelling, including relative paths,
/// symlinks, separators and dot components.
///
/// Equality compares spelling; use [`CanonicalPath`] to compare file identity.
/// APIs accepting a display path cannot receive a canonical identity. For
/// example, the signature used by `BackendError::with_state_read_location`:
///
/// ```compile_fail,E0308
/// use carina_core::paths::{CanonicalPath, DisplayPath};
/// fn with_state_read_location(location: &DisplayPath) {}
/// let p = CanonicalPath::new(".").unwrap();
/// with_state_read_location(&p);
/// ```
///
/// ```compile_fail,E0277
/// use carina_core::paths::DisplayPath;
/// let path = DisplayPath::new("main.crn").unwrap();
/// let _ = std::fs::read_to_string(&path);
/// ```
///
/// ```compile_fail,E0599
/// use carina_core::paths::DisplayPath;
/// let path = DisplayPath::new("main.crn").unwrap();
/// let _ = std::fs::read_to_string(path.as_path());
/// ```
#[derive(Debug, Clone, Serialize)]
#[serde(transparent)]
pub struct DisplayPath(PathBuf);

impl DisplayPath {
    pub fn new(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref();
        if path.as_os_str().is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "an empty path has no absolute IO address",
            ));
        }
        Ok(Self(path.to_path_buf()))
    }

    pub fn file_name(&self) -> Option<&std::ffi::OsStr> {
        self.0.file_name()
    }
    pub fn extension(&self) -> Option<&std::ffi::OsStr> {
        self.0.extension()
    }
    pub fn parent(&self) -> Option<Self> {
        self.0.parent().map(|path| Self(path.to_path_buf()))
    }
    /// Preserve the base's spelling, omitting `.` components only from the child.
    pub fn join(&self, child: impl AsRef<Path>) -> Self {
        let child: PathBuf = child
            .as_ref()
            .components()
            .filter(|component| *component != Component::CurDir)
            .collect();
        if child.as_os_str().is_empty() {
            self.clone()
        } else {
            Self(self.0.join(child))
        }
    }
    pub fn with_extension(&self, extension: impl AsRef<std::ffi::OsStr>) -> Self {
        Self(self.0.with_extension(extension))
    }
    /// Lexical relative rendering for source labels and command output only.
    pub fn relative_to(&self, base: &Self) -> Option<String> {
        strip_display_prefix(&self.0, &base.0).map(|path| path.display().to_string())
    }
}

// Joined labels may omit a leading `./` that an authored locator still has.
// Treat both spellings alike when locating them within a pinned display tree.
fn strip_display_prefix<'a>(path: &'a Path, base: &Path) -> Option<&'a Path> {
    let path = path.strip_prefix(".").unwrap_or(path);
    let base = base.strip_prefix(".").unwrap_or(base);
    path.strip_prefix(base).ok()
}

impl PartialEq for DisplayPath {
    fn eq(&self, other: &Self) -> bool {
        self.0.as_os_str() == other.0.as_os_str()
    }
}

impl Eq for DisplayPath {}

impl PartialOrd for DisplayPath {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for DisplayPath {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.as_os_str().cmp(other.0.as_os_str())
    }
}

impl std::hash::Hash for DisplayPath {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        std::hash::Hash::hash(self.0.as_os_str(), state);
    }
}

fn absolute_io_address(path: &Path) -> io::Result<PathBuf> {
    if path.is_relative()
        && !path.as_os_str().is_empty()
        && let Some(pwd) = std::env::var_os("PWD").map(PathBuf::from)
        && let Ok(current) = std::env::current_dir()
        && logical_pwd_matches_cwd(&pwd, &current)
    {
        return std::path::absolute(pwd.join(path));
    }
    std::path::absolute(path)
}

fn logical_pwd_matches_cwd(pwd: &Path, cwd: &Path) -> bool {
    pwd.is_absolute()
        && !pwd
            .components()
            .any(|part| matches!(part, Component::CurDir | Component::ParentDir))
        // `components` normalizes interior/trailing `.` segments away.
        && !pwd
            .as_os_str()
            .as_encoded_bytes()
            .split(|byte| byte.is_ascii() && std::path::is_separator(char::from(*byte)))
            .any(|part| part == b".")
        && matches!(
            (CanonicalPath::new(pwd), CanonicalPath::new(cwd)),
            (Ok(logical), Ok(physical)) if logical == physical
        )
}

impl fmt::Display for DisplayPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.display().fmt(f)
    }
}

impl<'de> Deserialize<'de> for DisplayPath {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let path = PathBuf::deserialize(deserializer)?;
        Self::new(path).map_err(serde::de::Error::custom)
    }
}

/// An absolute locator for recorded paths and later filesystem access.
///
/// Construction uses [`std::path::absolute`] without resolving symlinks or
/// parent components. Deserialization requires an already absolute spelling
/// and never consults the filesystem. Use [`ResolvedPath::persisted_path`] to
/// retain the base directory captured when a live path was resolved.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct PersistedPath(DisplayPath);

impl PersistedPath {
    pub fn new(path: impl AsRef<Path>) -> io::Result<Self> {
        let display = DisplayPath::new(path)?;
        std::path::absolute(&display.0).map(|path| Self(DisplayPath(path)))
    }

    pub fn display(&self) -> &DisplayPath {
        &self.0
    }

    pub fn join(&self, child: impl AsRef<Path>) -> io::Result<Self> {
        Self::new(self.0.join(child).0)
    }

    /// Render a relative selector from logical locators, without resolving
    /// symlinks. Dot and parent components are reduced lexically.
    pub fn lexical_relative_to(&self, base: &Self) -> Option<DisplayPath> {
        fn normalize(path: &Path) -> PathBuf {
            let mut normalized = PathBuf::new();
            for component in path.components() {
                match component {
                    Component::CurDir => {}
                    Component::ParentDir => {
                        normalized.pop();
                    }
                    component => normalized.push(component.as_os_str()),
                }
            }
            normalized
        }

        relative_path_from_root(&normalize(&base.0.0), &normalize(&self.0.0)).map(|relative| {
            DisplayPath(if relative.as_os_str().is_empty() {
                PathBuf::from(".")
            } else {
                relative
            })
        })
    }

    /// Pin the saved locator against the filesystem at the time it is reused.
    pub fn resolve(&self) -> io::Result<ResolvedPath> {
        ResolvedPath::new(&self.0.0)
    }
}

impl fmt::Display for PersistedPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

impl<'de> Deserialize<'de> for PersistedPath {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let path = PathBuf::deserialize(deserializer)?;
        if !path.is_absolute() {
            return Err(serde::de::Error::custom("persisted path must be absolute"));
        }
        Ok(Self(DisplayPath(path)))
    }
}

/// Best-effort identity for caches and cycle guards that also allow missing
/// paths. Failed canonicalization retains the original spelling as a fallback
/// key, without making an unresolved path into a `CanonicalPath`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PathIdentity(PathIdentityKind);

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum PathIdentityKind {
    Canonical(CanonicalPath),
    Unresolved(PathBuf),
}

impl From<CanonicalPath> for PathIdentity {
    fn from(identity: CanonicalPath) -> Self {
        Self(PathIdentityKind::Canonical(identity))
    }
}

impl PathIdentity {
    pub fn new(path: impl AsRef<Path>) -> Self {
        let path = path.as_ref();
        Self(match CanonicalPath::new(path) {
            Ok(path) => PathIdentityKind::Canonical(path),
            Err(_) => PathIdentityKind::Unresolved(path.to_path_buf()),
        })
    }

    /// Ancestor identities for provider-scope lookup, without exposing the
    /// resolved spelling to diagnostics.
    pub fn ancestors(&self) -> impl Iterator<Item = Self> + '_ {
        let (path, canonical) = match &self.0 {
            PathIdentityKind::Canonical(path) => (&path.0, true),
            PathIdentityKind::Unresolved(path) => (path, false),
        };
        path.ancestors().map(move |ancestor| {
            if canonical {
                Self(PathIdentityKind::Canonical(CanonicalPath(
                    ancestor.to_path_buf(),
                )))
            } else {
                Self::new(ancestor)
            }
        })
    }
}

fn relative_path_from_root(root: &Path, path: &Path) -> Option<PathBuf> {
    let root_components = root.components().collect::<Vec<_>>();
    let path_components = path.components().collect::<Vec<_>>();
    let anchor_len = |components: &[Component<'_>]| {
        components
            .iter()
            .take_while(|component| matches!(component, Component::Prefix(_) | Component::RootDir))
            .count()
    };
    let root_anchor_len = anchor_len(&root_components);
    let path_anchor_len = anchor_len(&path_components);
    if root_components[..root_anchor_len] != path_components[..path_anchor_len] {
        return None;
    }

    let common = root_components
        .iter()
        .zip(&path_components)
        .take_while(|(root, path)| root == path)
        .count();
    let mut relative = PathBuf::new();
    for _ in &root_components[common..] {
        relative.push(Component::ParentDir.as_os_str());
    }
    for component in &path_components[common..] {
        relative.push(component.as_os_str());
    }
    Some(relative)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persisted_paths_render_project_relative_selectors_lexically() {
        for (source, project, expected) in [
            ("project/mock-http.wasm", "project", "mock-http.wasm"),
            ("project/./alias.wasm", "./project", "alias.wasm"),
            ("./alias.wasm", ".", "alias.wasm"),
            ("project/nested/../alias.wasm", "project", "alias.wasm"),
            ("project/alias.wasm", "project/nested/..", "alias.wasm"),
            ("shared/provider.wasm", "project", "../shared/provider.wasm"),
            ("project/../provider.wasm", "project", "../provider.wasm"),
            ("project", "project", "."),
        ] {
            let source = PersistedPath::new(source).unwrap();
            let project = PersistedPath::new(project).unwrap();
            assert_eq!(
                source.lexical_relative_to(&project).unwrap().to_string(),
                expected,
                "source={source}, project={project}"
            );
        }
    }

    #[test]
    fn display_path_construction_and_serde_preserve_authored_spelling() {
        for input in [
            ".",
            "./main.crn",
            "infra//./foo/../main.crn",
            "//missing-project/main.crn",
        ] {
            let path = DisplayPath::new(input).unwrap();
            assert_eq!(path.to_string(), input);
            let encoded = serde_json::to_value(&path).unwrap();
            assert_eq!(encoded, input);
            let decoded: DisplayPath = serde_json::from_value(encoded).unwrap();
            assert_eq!(decoded.to_string(), input);
        }
    }

    #[test]
    fn display_path_joins_preserve_base_spelling_and_omit_child_curdir_components() {
        for (base, child, expected) in [
            (".", "main.crn", "./main.crn"),
            (".", "../upstream", "./../upstream"),
            ("./", "./../upstream", "./../upstream"),
            ("./.", "./main.crn", "././main.crn"),
            ("infra/env", "./alias/main.crn", "infra/env/alias/main.crn"),
            ("./infra/env", "./alias", "./infra/env/alias"),
            (
                "infra//./env",
                "./alias/./main.crn",
                "infra//./env/alias/main.crn",
            ),
            ("infra/env", "../upstream", "infra/env/../upstream"),
            (
                "infra/env",
                "alias/../main.crn",
                "infra/env/alias/../main.crn",
            ),
            ("infra/env", ".", "infra/env"),
            (".", ".", "."),
            ("infra/env", "", "infra/env"),
            ("infra/env", "/other/main.crn", "/other/main.crn"),
        ] {
            assert_eq!(
                DisplayPath::new(base).unwrap().join(child).to_string(),
                expected
            );
        }

        let base = PersistedPath::new("infra/env").unwrap();
        assert_eq!(
            base.join("./alias/../main.crn").unwrap().to_string(),
            format!("{base}/alias/../main.crn")
        );
        assert_eq!(
            base.join("../upstream").unwrap().0.0,
            std::path::absolute(base.0.0.join("../upstream")).unwrap()
        );
    }

    #[test]
    fn display_path_relative_labels_accept_an_optional_leading_dot() {
        for (base, path, expected) in [
            (".", "../upstream", "../upstream"),
            (".", "./main.crn", "main.crn"),
            ("infra/env", "./infra/env/main.crn", "main.crn"),
            ("./infra/env", "infra/env/main.crn", "main.crn"),
            (
                ".carina/providers",
                "./.carina/providers/mock.wasm",
                "mock.wasm",
            ),
        ] {
            assert_eq!(
                DisplayPath::new(path)
                    .unwrap()
                    .relative_to(&DisplayPath::new(base).unwrap()),
                Some(expected.to_string())
            );
        }
    }

    #[test]
    fn joined_dot_display_locators_reuse_the_pinned_project() {
        let project = tempfile::tempdir().unwrap();
        std::fs::write(project.path().join("main.crn"), "pinned project").unwrap();
        // Model a captured logical CWD without changing the test process CWD.
        let root = ResolvedPath::from_address(
            project.path(),
            DisplayPath::new(".").unwrap(),
            project.path().to_path_buf(),
            PersistedPath::new(project.path()).unwrap(),
        )
        .unwrap();
        let child = root.join("./main.crn").unwrap();
        assert_eq!(child.display().to_string(), "./main.crn");
        let located = root.join_display(child.display()).unwrap();
        assert_eq!(located.read_to_string().unwrap(), "pinned project");
        assert_eq!(located.persisted_path(), child.persisted_path());
    }

    #[cfg(unix)]
    #[test]
    fn display_path_ordering_compares_spelling_including_separators() {
        let single = DisplayPath::new("/project/main.crn").unwrap();
        let double = DisplayPath::new("//project/main.crn").unwrap();
        assert_ne!(single, double);
        assert!(double < single);
        let spellings = std::collections::HashSet::from([single.clone(), double.clone()]);
        assert_eq!(spellings.len(), 2);
        for paths in [[single.clone(), double.clone()], [double.clone(), single]] {
            assert_eq!(paths.into_iter().min().unwrap(), double);
        }
    }

    #[cfg(unix)]
    #[test]
    fn resolved_path_relative_display_locators_keep_io_pinned_without_repeating_root() {
        let cwd = std::env::current_dir().unwrap();
        let temp = tempfile::tempdir_in(&cwd).unwrap();
        let original = temp.path().join("original");
        let replacement = temp.path().join("replacement");
        let alias = temp.path().join("alias");
        std::fs::create_dir(&original).unwrap();
        std::fs::create_dir(&replacement).unwrap();
        std::fs::write(original.join("main.crn"), "original").unwrap();
        std::fs::write(replacement.join("main.crn"), "replacement").unwrap();
        std::fs::write(temp.path().join("outside.crn"), "outside").unwrap();
        std::os::unix::fs::symlink(&original, &alias).unwrap();
        let relative_temp = Path::new(".").join(temp.path().file_name().unwrap());
        let relative_alias = relative_temp.join("alias");
        let root = ResolvedPath::new(&relative_alias).unwrap();
        std::fs::remove_file(&alias).unwrap();
        std::os::unix::fs::symlink(&replacement, &alias).unwrap();

        for spelling in [relative_alias.join("main.crn"), alias.join("main.crn")] {
            let label = DisplayPath::new(&spelling).unwrap();
            let source = root.join_display(&label).unwrap();
            assert_eq!(source.read_to_string().unwrap(), "original");
            assert_eq!(source.display().to_string(), spelling.display().to_string());
        }
        let outside = DisplayPath::new(relative_temp.join("outside.crn")).unwrap();
        assert_eq!(
            root.join_display(&outside)
                .unwrap()
                .read_to_string()
                .unwrap(),
            "outside"
        );
        assert_eq!(
            root.join("main.crn").unwrap().display().to_string(),
            relative_alias.join("main.crn").display().to_string()
        );
    }

    #[cfg(unix)]
    #[test]
    fn resolved_path_symlink_metadata_keeps_original_entry_after_alias_retarget() {
        let temp = tempfile::tempdir().unwrap();
        let original = temp.path().join("original");
        let replacement = temp.path().join("replacement");
        let alias = temp.path().join("alias");
        std::fs::create_dir(&original).unwrap();
        std::fs::create_dir(&replacement).unwrap();
        std::os::unix::fs::symlink("missing", original.join("input")).unwrap();
        std::fs::write(replacement.join("input"), "replacement").unwrap();
        std::os::unix::fs::symlink(&original, &alias).unwrap();
        let input = ResolvedPath::new(alias.join("input")).unwrap();
        std::fs::remove_file(&alias).unwrap();
        std::os::unix::fs::symlink(&replacement, &alias).unwrap();

        assert!(input.symlink_metadata().unwrap().file_type().is_symlink());
        assert_eq!(
            input.metadata().unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
    }

    #[cfg(unix)]
    #[test]
    fn resolved_path_symlink_retarget_keeps_children_and_atomic_io_pinned() {
        let temp = tempfile::tempdir().unwrap();
        let original = temp.path().join("original");
        let replacement = temp.path().join("replacement");
        let alias = temp.path().join("alias");
        std::fs::create_dir(&original).unwrap();
        std::fs::create_dir(&replacement).unwrap();
        std::fs::write(original.join("main.crn"), "original").unwrap();
        std::fs::write(replacement.join("main.crn"), "replacement").unwrap();
        std::os::unix::fs::symlink(&original, &alias).unwrap();
        let root = ResolvedPath::new(&alias).unwrap();
        let identity = root.identity();
        std::fs::remove_file(&alias).unwrap();
        std::os::unix::fs::symlink(&replacement, &alias).unwrap();

        assert_eq!(root.identity(), identity);
        let files = root.crn_files().unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].read_to_string().unwrap(), "original");
        assert_eq!(files[0].display().0.as_path(), alias.join("main.crn"));
        let state = root.join("state.json").unwrap();
        let lock = root.join("state.lock").unwrap();
        lock.create_lock(b"owner").unwrap();
        assert_eq!(
            lock.create_lock(b"thief").unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        state.atomic_replace(b"state").unwrap();
        lock.remove().unwrap();
        assert_eq!(
            std::fs::read(original.join("state.json")).unwrap(),
            b"state"
        );
        assert!(!replacement.join("state.json").exists());
        assert!(!original.join("state.lock").exists());
    }

    #[cfg(unix)]
    #[test]
    fn resolved_path_symlink_child_and_missing_target_are_pinned_at_join() {
        let temp = tempfile::tempdir().unwrap();
        let original = temp.path().join("original");
        let replacement = temp.path().join("replacement");
        std::fs::create_dir(&original).unwrap();
        std::fs::create_dir(&replacement).unwrap();
        std::os::unix::fs::symlink(&original, temp.path().join("child")).unwrap();
        std::os::unix::fs::symlink("child/missing.json", temp.path().join("state.json")).unwrap();
        let root = ResolvedPath::new(temp.path()).unwrap();
        let child = root.join("child").unwrap();
        let state = root.join("state.json").unwrap();
        std::fs::remove_file(temp.path().join("child")).unwrap();
        std::os::unix::fs::symlink(&replacement, temp.path().join("child")).unwrap();
        child
            .join("main.crn")
            .unwrap()
            .atomic_replace(b"original")
            .unwrap();
        state.atomic_replace(b"pinned").unwrap();
        assert_eq!(
            std::fs::read(original.join("missing.json")).unwrap(),
            b"pinned"
        );
        assert!(!replacement.join("missing.json").exists());
        assert!(!replacement.join("main.crn").exists());
        assert!(
            std::fs::symlink_metadata(temp.path().join("state.json"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[cfg(unix)]
    #[test]
    fn state_and_lock_share_the_parent_pinned_before_a_symlink_retarget() {
        for existing_state in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let original = temp.path().join("original");
            let replacement = temp.path().join("replacement");
            let current = temp.path().join("current");
            std::fs::create_dir(&original).unwrap();
            std::fs::create_dir(&replacement).unwrap();
            if existing_state {
                std::fs::write(original.join("carina.state.json"), b"original").unwrap();
                std::fs::write(replacement.join("carina.state.json"), b"replacement").unwrap();
            }
            std::os::unix::fs::symlink(&original, &current).unwrap();
            let address = current.join("carina.state.json");
            let state = ResolvedPath::from_address_after_parent_pin(
                &address,
                DisplayPath::new(&address).unwrap(),
                address.clone(),
                PersistedPath::new(&address).unwrap(),
                || {
                    std::fs::remove_file(&current).unwrap();
                    std::os::unix::fs::symlink(&replacement, &current).unwrap();
                },
            )
            .unwrap();
            let lock = state.with_extension("lock").unwrap();
            lock.create_lock(b"owner").unwrap();
            if existing_state {
                assert_eq!(state.read().unwrap(), b"original");
            }
            state.atomic_replace(b"updated").unwrap();
            assert_eq!(
                std::fs::read(original.join("carina.state.json")).unwrap(),
                b"updated"
            );
            assert!(original.join("carina.state.lock").exists());
            assert!(!replacement.join("carina.state.lock").exists());
            if existing_state {
                assert_eq!(
                    std::fs::read(replacement.join("carina.state.json")).unwrap(),
                    b"replacement"
                );
            } else {
                assert!(!replacement.join("carina.state.json").exists());
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn resolved_path_with_extension_keeps_lock_beside_displayed_state_symlink() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        let data = temp.path().join("data");
        std::fs::create_dir(&project).unwrap();
        std::fs::create_dir(&data).unwrap();
        std::fs::write(data.join("real.json"), b"state").unwrap();
        std::os::unix::fs::symlink(data.join("real.json"), project.join("carina.state.json"))
            .unwrap();
        let state = ResolvedPath::new(project.join("carina.state.json")).unwrap();
        let lock = state.with_extension("lock").unwrap();
        lock.create_lock(b"owner").unwrap();
        assert!(project.join("carina.state.lock").exists());
        assert!(!data.join("real.lock").exists());
    }

    #[cfg(unix)]
    #[test]
    fn resolved_path_atomic_replace_preserves_private_file_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("bootstrap.crn");
        std::fs::write(&target, "old").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();
        let path = ResolvedPath::new(&target).unwrap();
        path.atomic_replace(b"new").unwrap();
        assert_eq!(path.read().unwrap(), b"new");
        assert_eq!(path.metadata().unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 1);
    }

    #[test]
    fn logical_pwd_rejects_dot_components() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("nested")).unwrap();
        let accepted = ["nested/..", ".", "./nested/..", "nested/../."]
            .into_iter()
            .filter(|suffix| logical_pwd_matches_cwd(&dir.path().join(suffix), dir.path()))
            .collect::<Vec<_>>();
        assert!(
            accepted.is_empty(),
            "accepted non-normalized PWDs: {accepted:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn logical_pwd_requires_absolute_cwd_identity_and_preserves_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        let link = dir.path().join("link");
        std::fs::create_dir(&real).unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();

        assert!(logical_pwd_matches_cwd(&real, &real));
        assert!(logical_pwd_matches_cwd(&link, &real));
        assert!(!logical_pwd_matches_cwd(&link, dir.path()));
        assert!(!logical_pwd_matches_cwd(Path::new("."), &real));
        assert!(!logical_pwd_matches_cwd(&dir.path().join("missing"), &real));
    }

    #[test]
    fn display_path_preserves_relative_input_spelling() {
        let input = "./project//main.crn";
        let path = DisplayPath::new(input).unwrap();
        assert!(path.0.is_relative());
        assert_eq!(path.to_string(), input);
    }

    #[test]
    fn persisted_path_makes_relative_input_absolute() {
        let input = "project/main.crn";
        let path = PersistedPath::new(input).unwrap();
        assert!(path.0.0.is_absolute());
        assert_eq!(path.0.0, std::path::absolute(input).unwrap());
        assert_eq!(
            CanonicalPath::new(path.0.0.parent().unwrap().parent().unwrap()).unwrap(),
            CanonicalPath::new(std::env::current_dir().unwrap()).unwrap()
        );
    }

    #[test]
    fn persisted_path_deserialization_rejects_relative_paths() {
        for input in ["project/main.crn", "../project", ""] {
            let error = serde_json::from_value::<PersistedPath>(serde_json::json!(input))
                .expect_err("persisted paths must already be absolute");
            assert!(error.to_string().contains("absolute"), "{error}");
        }
    }

    #[test]
    fn display_path_deserialization_preserves_absolute_spelling() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("nonexistent/../main.crn");
        let path: DisplayPath = serde_json::from_value(serde_json::json!(input)).unwrap();
        assert_eq!(path.0.as_path(), input);
    }

    #[test]
    fn persisted_path_deserialization_preserves_absolute_spelling() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("nonexistent/../main.crn");
        let path: PersistedPath = serde_json::from_value(serde_json::json!(input)).unwrap();
        assert_eq!(path.0.0, input);
        assert_eq!(
            serde_json::to_value(path).unwrap(),
            serde_json::json!(input)
        );
    }

    #[cfg(unix)]
    #[test]
    fn persisted_path_from_resolved_retains_symlink_locator_after_retarget() {
        let cwd = std::env::current_dir().unwrap();
        let dir = tempfile::tempdir_in(&cwd).unwrap();
        let original = dir.path().join("original");
        let replacement = dir.path().join("replacement");
        let alias = dir.path().join("alias");
        std::fs::create_dir(&original).unwrap();
        std::fs::create_dir(&replacement).unwrap();
        std::fs::write(original.join("main.crn"), "original").unwrap();
        std::fs::write(replacement.join("main.crn"), "replacement").unwrap();
        std::os::unix::fs::symlink(&original, &alias).unwrap();
        let input = Path::new(".")
            .join(dir.path().file_name().unwrap())
            .join("alias");
        let resolved = ResolvedPath::new(&input).unwrap();
        let saved = resolved.persisted_path().clone();
        assert_eq!(resolved.display().to_string(), input.display().to_string());
        assert_eq!(saved.0.0, std::path::absolute(&input).unwrap());
        std::fs::remove_file(&alias).unwrap();
        std::os::unix::fs::symlink(&replacement, &alias).unwrap();

        assert_eq!(resolved.persisted_path(), &saved);
        assert_eq!(resolved.identity(), CanonicalPath::new(&original).unwrap());
        assert_eq!(
            resolved
                .join_display(&saved.display().join("main.crn"))
                .unwrap()
                .read_to_string()
                .unwrap(),
            "original"
        );
        assert_eq!(
            saved.resolve().unwrap().identity(),
            CanonicalPath::new(&replacement).unwrap()
        );
        assert!(saved.to_string().ends_with("/alias"));
    }

    #[test]
    fn canonical_ancestors_retain_identity_after_directory_removal() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("nested");
        std::fs::create_dir(&nested).unwrap();
        let identity = PathIdentity::new(&nested);
        let parent = PathIdentity::new(dir.path());
        std::fs::remove_dir(&nested).unwrap();

        let mut ancestors = identity.ancestors();
        assert_eq!(ancestors.next(), Some(identity.clone()));
        assert_eq!(ancestors.next(), Some(parent));
    }

    #[test]
    fn display_path_preserves_parent_components_without_filesystem_access() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("nonexistent/../main.crn");
        let path = DisplayPath::new(&input).unwrap();
        assert_eq!(path.0.as_path(), input);
        assert_eq!(path.to_string(), input.display().to_string());
    }

    #[cfg(unix)]
    #[test]
    fn display_path_preserves_symlink_and_parent_components() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real/nested");
        let link = dir.path().join("link");
        std::fs::create_dir_all(&real).unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let input = link.join("..");

        let display = DisplayPath::new(&input).unwrap();
        assert_eq!(display.0.as_path(), input);
        assert!(display.to_string().contains("/link/.."));
        assert!(!display.to_string().contains("/real"));
        assert_eq!(
            CanonicalPath::new(display.0.as_path()).unwrap(),
            CanonicalPath::new(dir.path().join("real")).unwrap()
        );
    }

    #[test]
    fn canonical_path_debug_does_not_reveal_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = CanonicalPath::new(dir.path()).unwrap();
        assert_eq!(format!("{path:?}"), "CanonicalPath(..)");
    }

    #[test]
    fn canonical_path_serde_retains_identity_after_source_removal() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        std::fs::create_dir(&source).unwrap();
        let identity = CanonicalPath::new(&source).unwrap();
        let serialized = serde_json::to_string(&identity).unwrap();
        std::fs::remove_dir(&source).unwrap();
        let restored: CanonicalPath = serde_json::from_str(&serialized).unwrap();
        assert_eq!(restored, identity);
        assert_eq!(format!("{restored:?}"), "CanonicalPath(..)");
    }

    #[test]
    fn canonical_path_deserialization_rejects_noncanonical_spelling() {
        let root = tempfile::tempdir().unwrap();
        for path in [PathBuf::from("relative"), root.path().join("a/../b")] {
            assert!(serde_json::from_value::<CanonicalPath>(serde_json::json!(path)).is_err());
        }
    }
}
