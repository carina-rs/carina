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
        Self::from_address(&display.0, display.clone())
    }

    fn from_address(address: &Path, display: DisplayPath) -> io::Result<Self> {
        let sibling_parent = address
            .parent()
            .map(|parent| pin_io_path(parent, 0))
            .transpose()?;
        let resolved = pin_io_path(address, 0)?;
        Ok(Self {
            resolved,
            display,
            sibling_parent,
        })
    }

    pub fn parent(&self) -> Option<Self> {
        let resolved = self.sibling_parent.clone()?;
        Some(Self {
            sibling_parent: resolved.parent().map(Path::to_path_buf),
            resolved,
            display: DisplayPath(self.display.0.parent()?.to_path_buf()),
        })
    }

    /// Join an absolute diagnostic label without exporting it as an IO path.
    pub fn join_display(&self, child: &DisplayPath) -> io::Result<Self> {
        self.join(&child.0)
    }

    pub fn display(&self) -> &DisplayPath {
        &self.display
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
            child.strip_prefix(&self.display.0).unwrap_or(child)
        } else {
            child
        };
        Self::from_address(
            &self.resolved.join(relative),
            DisplayPath(self.display.0.join(child)),
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
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct CanonicalPath(PathBuf);

impl CanonicalPath {
    pub fn ancestors(&self) -> impl Iterator<Item = Self> + '_ {
        self.0.ancestors().map(|path| Self(path.to_path_buf()))
    }

    pub fn relative_identity(&self, root: &Self) -> Option<String> {
        relative_path_from_root(&root.0, &self.0).map(|path| path.display().to_string())
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

/// An absolute path retaining symlinks and parent components as supplied.
///
/// Relative input uses a validated logical `$PWD`, falling back to
/// [`std::path::absolute`] when it does not identify the current directory.
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
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct DisplayPath(PathBuf);

impl DisplayPath {
    pub fn new(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref();
        if path.is_relative()
            && !path.as_os_str().is_empty()
            && let Some(pwd) = std::env::var_os("PWD").map(PathBuf::from)
            && let Ok(current) = std::env::current_dir()
            && logical_pwd_matches_cwd(&pwd, &current)
        {
            return std::path::absolute(pwd.join(path)).map(Self);
        }
        std::path::absolute(path).map(Self)
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
    pub fn join(&self, child: impl AsRef<Path>) -> Self {
        Self(self.0.join(child))
    }
    pub fn with_extension(&self, extension: impl AsRef<std::ffi::OsStr>) -> Self {
        Self(self.0.with_extension(extension))
    }
    /// Lexical relative rendering for source labels and command output only.
    pub fn relative_to(&self, base: &Self) -> Option<String> {
        self.0
            .strip_prefix(&base.0)
            .ok()
            .map(|path| path.display().to_string())
    }
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
        if !path.is_absolute() {
            return Err(serde::de::Error::custom("display path must be absolute"));
        }
        Ok(Self(path))
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
    fn display_path_makes_relative_input_absolute() {
        let path = DisplayPath::new("project/main.crn").unwrap();
        assert!(path.0.as_path().is_absolute());
        assert!(path.0.as_path().ends_with("project/main.crn"));
        assert_eq!(
            CanonicalPath::new(path.0.as_path().parent().unwrap().parent().unwrap()).unwrap(),
            CanonicalPath::new(std::env::current_dir().unwrap()).unwrap()
        );
    }

    #[test]
    fn display_path_deserialization_rejects_relative_paths() {
        for input in ["project/main.crn", "../project", ""] {
            let error = serde_json::from_value::<DisplayPath>(serde_json::json!(input))
                .expect_err("persisted display paths must already be absolute");
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
