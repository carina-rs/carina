use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, Write};
use std::ops::{Deref, DerefMut};
use std::path::{Component, Path, PathBuf};

use carina_core::UserDirs;
use tempfile::NamedTempFile;

use crate::provider_resolver::{
    LockEntry, LockFile, LockFileError, LockFileMigration, WasmArtifactPath, provider_lock_path,
    resolve_lock_file_save_path, resolve_parent, sha256_file,
};
use crate::verified_artifact::{VerifiedArtifact, VerifiedArtifactError, publish_cached};

const PROVIDER_INSTALL_LOCK_FILE_NAME: &str = "provider-install.lock";

/// Exclusive ownership of provider installation and lock-file mutation for one project.
pub struct ProviderInstallLock {
    _guard_file: File,
    project_dir: PathBuf,
}

/// Provider lock state loaded, or defaulted, while its project's install lock is held.
///
/// The value borrows the install lock, so it can only be saved while that lock
/// remains held. Its contents can still be replaced through [`DerefMut`].
pub struct GuardedLockFile<'lock> {
    install_lock: &'lock ProviderInstallLock,
    lock_file: LockFile,
    migration: Option<LockFileMigration>,
}

impl<'lock> GuardedLockFile<'lock> {
    /// Remove and return the migration event produced while loading this lock.
    pub fn take_migration(&mut self) -> Option<LockFileMigration> {
        self.migration.take()
    }

    pub(crate) fn report_migration(&mut self) {
        if let Some(migration) = self.take_migration() {
            eprintln!("{migration}");
        }
    }

    /// Return the project directory protected by this lock file's install lock.
    pub fn project_dir(&self) -> &Path {
        self.install_lock.project_dir()
    }

    pub(crate) fn install_lock(&self) -> &'lock ProviderInstallLock {
        self.install_lock
    }

    pub(crate) fn clone_for_save(&self) -> GuardedLockFile<'lock> {
        GuardedLockFile {
            install_lock: self.install_lock,
            lock_file: self.lock_file.clone(),
            migration: None,
        }
    }

    pub fn save(&self) -> io::Result<()> {
        save_lock_file_to_path(
            &self.lock_file,
            &provider_lock_path(self.install_lock.project_dir()),
        )
    }

    #[cfg(test)]
    pub(crate) fn into_lock_file_for_test(self) -> LockFile {
        self.lock_file
    }
}

impl Deref for GuardedLockFile<'_> {
    type Target = LockFile;

    fn deref(&self) -> &Self::Target {
        &self.lock_file
    }
}

impl DerefMut for GuardedLockFile<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.lock_file
    }
}

impl ProviderInstallLock {
    /// Acquires the exclusive provider-install lock for `project_dir`.
    ///
    /// This lock is not re-entrant. Acquiring it a second time for the same
    /// project in the same process while the first value is alive blocks
    /// forever because `flock` applies to each open file description.
    pub fn acquire(project_dir: &Path) -> io::Result<Self> {
        let metadata = fs::metadata(project_dir).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!(
                    "Failed to access project directory {}: {error}",
                    project_dir.display()
                ),
            )
        })?;
        if !metadata.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::NotADirectory,
                format!("Project path {} is not a directory", project_dir.display()),
            ));
        }

        let carina_dir = project_dir.join(".carina");
        match fs::create_dir(&carina_dir) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        let guard_path = carina_dir.join(PROVIDER_INSTALL_LOCK_FILE_NAME);
        let guard_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&guard_path)?;

        match guard_file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                eprintln!(
                    "Waiting for provider install lock at {} (held by another carina process)...",
                    guard_path.display()
                );
                guard_file.lock()?;
            }
            Err(TryLockError::Error(error)) => return Err(error),
        }

        Ok(Self {
            _guard_file: guard_file,
            project_dir: project_dir.to_path_buf(),
        })
    }

    pub fn project_dir(&self) -> &Path {
        &self.project_dir
    }

    pub fn load_lock_file(&self) -> Result<Option<GuardedLockFile<'_>>, LockFileError> {
        LockFile::load(self.project_dir()).map(|loaded| {
            loaded.map(|loaded| {
                let (lock_file, migration) = loaded.into_parts();
                GuardedLockFile {
                    install_lock: self,
                    lock_file,
                    migration,
                }
            })
        })
    }

    pub(crate) fn load_lock_file_or_default(&self) -> Result<GuardedLockFile<'_>, LockFileError> {
        self.load_lock_file().map(|loaded| {
            loaded.unwrap_or_else(|| GuardedLockFile {
                install_lock: self,
                lock_file: LockFile::default(),
                migration: None,
            })
        })
    }

    #[cfg(test)]
    pub(crate) fn guard_lock_file_for_test(&self, lock_file: LockFile) -> GuardedLockFile<'_> {
        GuardedLockFile {
            install_lock: self,
            lock_file,
            migration: None,
        }
    }

    pub(crate) fn remove_project_artifact(&self, artifact: &WasmArtifactPath) -> io::Result<()> {
        validate_project_target(self, artifact.as_path())?;
        fs::remove_file(artifact.as_path())
    }
}

enum ArtifactTarget<'a> {
    Project {
        _install_lock: &'a ProviderInstallLock,
        path: PathBuf,
    },
    Global(PathBuf),
}

impl ArtifactTarget<'_> {
    fn path(&self) -> &Path {
        match self {
            Self::Project {
                _install_lock: _,
                path,
            }
            | Self::Global(path) => path,
        }
    }
}

/// A uniquely named artifact staged beside its final destination.
///
/// The destination is private and publication consumes the stage, so callers can
/// only make bytes visible through the atomic rename implemented here.
pub(crate) struct StagedArtifact<'a> {
    file: StagedFile,
    install_lock: &'a ProviderInstallLock,
    destination: WasmArtifactPath,
}

struct StagedFile {
    file: NamedTempFile,
    needs_data_sync: bool,
}

impl StagedFile {
    fn writable(file: NamedTempFile) -> Self {
        Self {
            file,
            needs_data_sync: true,
        }
    }

    fn hard_link(file: NamedTempFile) -> Self {
        Self {
            file,
            needs_data_sync: false,
        }
    }

    fn path(&self) -> &Path {
        self.file.path()
    }

    fn sync_data_if_needed(&self) -> io::Result<()> {
        if self.needs_data_sync {
            self.file.as_file().sync_all()?;
        }
        Ok(())
    }
}

pub(crate) struct GlobalArtifactPath(PathBuf);

impl GlobalArtifactPath {
    pub(crate) fn as_path(&self) -> &Path {
        &self.0
    }
}

pub(crate) fn global_cache_artifact_path(
    user_dirs: &UserDirs,
    relative: PathBuf,
) -> Option<GlobalArtifactPath> {
    if relative.components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        return None;
    }
    Some(GlobalArtifactPath(
        user_dirs.plugin_cache_dir().join(relative),
    ))
}

impl<'a> StagedArtifact<'a> {
    pub(crate) fn for_project(
        install_lock: &'a ProviderInstallLock,
        destination: &WasmArtifactPath,
    ) -> io::Result<Self> {
        validate_project_target(install_lock, destination.as_path())?;
        let parent = resolve_parent(destination.as_path());
        fs::create_dir_all(parent)?;

        #[cfg(unix)]
        let file = {
            use std::os::unix::fs::PermissionsExt;

            tempfile::Builder::new()
                .permissions(fs::Permissions::from_mode(0o666))
                .tempfile_in(parent)?
        };

        #[cfg(not(unix))]
        let file = NamedTempFile::new_in(parent)?;

        Ok(Self {
            file: StagedFile::writable(file),
            install_lock,
            destination: destination.clone(),
        })
    }

    fn from_source_for_project(
        install_lock: &'a ProviderInstallLock,
        source: &Path,
        destination: &WasmArtifactPath,
    ) -> io::Result<Self> {
        validate_project_target(install_lock, destination.as_path())?;
        let parent = resolve_parent(destination.as_path());
        fs::create_dir_all(parent)?;
        let file = stage_artifact_with(source, parent, |source, temporary_path| {
            fs::hard_link(source, temporary_path)
        })?;
        Ok(Self {
            file,
            install_lock,
            destination: destination.clone(),
        })
    }

    pub(crate) fn path(&self) -> &Path {
        self.file.path()
    }

    pub(crate) fn destination(&self) -> &WasmArtifactPath {
        &self.destination
    }

    pub(crate) fn project_dir(&self) -> &Path {
        self.install_lock.project_dir()
    }

    pub(crate) fn publish(self) -> io::Result<()> {
        self.file.sync_data_if_needed()?;
        let parent = resolve_parent(self.destination.as_path()).to_path_buf();
        publish_staged_file(self.file.file, self.destination.as_path())?;
        sync_parent_directory(&parent);
        Ok(())
    }
}

impl Write for StagedArtifact<'_> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.file.file.write(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.file.flush()
    }
}

pub(crate) fn install_project_artifact_atomically(
    install_lock: &ProviderInstallLock,
    source: &Path,
    destination: &WasmArtifactPath,
) -> io::Result<String> {
    let target = ArtifactTarget::Project {
        _install_lock: install_lock,
        path: destination.as_path().to_path_buf(),
    };
    validate_project_target(install_lock, target.path())?;
    install_artifact_atomically(source, target)
}

pub(crate) fn install_project_artifact_from_global_cache<R>(
    install_lock: &ProviderInstallLock,
    source: &Path,
    destination: &WasmArtifactPath,
    lock_entry: &LockEntry<R>,
    provider_name: &str,
) -> Result<VerifiedArtifact, VerifiedArtifactError> {
    let staged = StagedArtifact::from_source_for_project(install_lock, source, destination)
        .map_err(|error| {
            VerifiedArtifactError::Other(format!("Failed to link/copy from global cache: {error}"))
        })?;
    publish_cached(staged, lock_entry, provider_name)
}

pub(crate) fn install_global_artifact_atomically(
    source: &Path,
    destination: &GlobalArtifactPath,
) -> io::Result<()> {
    install_artifact_atomically(
        source,
        ArtifactTarget::Global(destination.as_path().to_path_buf()),
    )
    .map(|_| ())
}

fn validate_project_target(
    install_lock: &ProviderInstallLock,
    destination: &Path,
) -> io::Result<()> {
    let provider_cache = install_lock.project_dir().join(".carina").join("providers");
    if destination
        .strip_prefix(&provider_cache)
        .is_ok_and(|relative| {
            !relative.components().any(|component| {
                matches!(
                    component,
                    Component::ParentDir | Component::RootDir | Component::Prefix(_)
                )
            })
        })
    {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "project provider artifact {} is outside {}",
                destination.display(),
                provider_cache.display()
            ),
        ))
    }
}

fn install_artifact_atomically(source: &Path, target: ArtifactTarget<'_>) -> io::Result<String> {
    install_artifact_atomically_with(source, target, |source, destination| {
        fs::hard_link(source, destination)
    })
}

fn install_artifact_atomically_with(
    source: &Path,
    target: ArtifactTarget<'_>,
    link: impl FnOnce(&Path, &Path) -> io::Result<()>,
) -> io::Result<String> {
    let parent = resolve_parent(target.path()).to_path_buf();
    fs::create_dir_all(&parent)?;

    if is_same_file(source, target.path())? {
        return sha256_file(source);
    }

    let stage = stage_artifact_with(source, &parent, link)?;
    stage.sync_data_if_needed()?;
    let hash = sha256_file(stage.path())?;
    publish_staged_file(stage.file, target.path())?;
    sync_parent_directory(&parent);
    Ok(hash)
}

fn publish_staged_file(stage: NamedTempFile, destination: &Path) -> io::Result<()> {
    let staged_path = stage.path().to_path_buf();
    stage.persist(destination).map_err(|error| error.error)?;
    match is_same_file(&staged_path, destination) {
        Ok(true) => match fs::remove_file(staged_path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        },
        Ok(false) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    Ok(())
}

fn stage_artifact_with(
    source: &Path,
    parent: &Path,
    link: impl FnOnce(&Path, &Path) -> io::Result<()>,
) -> io::Result<StagedFile> {
    let placeholder = NamedTempFile::new_in(parent)?;
    let temporary_path = placeholder.into_temp_path();
    fs::remove_file(&temporary_path)?;

    match link(source, &temporary_path) {
        Ok(()) => {
            let file = File::open(&temporary_path)?;
            Ok(StagedFile::hard_link(NamedTempFile::from_parts(
                file,
                temporary_path,
            )))
        }
        Err(link_error) => {
            drop(temporary_path);
            (|| -> io::Result<StagedFile> {
                let mut stage = NamedTempFile::new_in(parent)?;
                let mut source_file = File::open(source)?;
                let source_permissions = source_file.metadata()?.permissions();
                io::copy(&mut source_file, &mut stage)?;
                stage.as_file().set_permissions(source_permissions)?;
                Ok(StagedFile::writable(stage))
            })()
            .map_err(|copy_error| {
                io::Error::new(
                    copy_error.kind(),
                    format!(
                        "hard link failed: {link_error}; staged copy fallback failed: {copy_error}"
                    ),
                )
            })
        }
    }
}

#[cfg(unix)]
fn is_same_file(source: &Path, destination: &Path) -> io::Result<bool> {
    use std::os::unix::fs::MetadataExt;

    let source = fs::symlink_metadata(source)?;
    let destination = match fs::symlink_metadata(destination) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    Ok(source.dev() == destination.dev() && source.ino() == destination.ino())
}

#[cfg(not(unix))]
fn is_same_file(_source: &Path, _destination: &Path) -> io::Result<bool> {
    Ok(false)
}

fn sync_parent_directory(parent: &Path) {
    if let Ok(parent_dir) = File::open(parent) {
        let _ = parent_dir.sync_all();
    }
}

fn save_lock_file_to_path(lock_file: &LockFile, path: &Path) -> io::Result<()> {
    // rename replaces a symlink itself, unlike fs::write. Resolve the target
    // first so every durability and replacement operation occurs in the target
    // directory while the caller's symlink remains intact.
    // It requires delete/replace permission for the target name, so a
    // deny-delete ACL intentionally makes save fail: no atomic replacement
    // can leave that name untouched.
    let path = resolve_lock_file_save_path(path)?;
    let parent = resolve_parent(&path);
    let content = lock_file
        .to_toml_string()
        .map_err(|error| io::Error::other(format!("Failed to serialize lock file: {error}")))?;

    #[cfg(unix)]
    let mut temp_file = {
        use std::os::unix::fs::PermissionsExt;

        let existing_permissions = match fs::metadata(&path) {
            Ok(metadata) => Some(metadata.permissions()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        let mut builder = tempfile::Builder::new();
        builder.permissions(fs::Permissions::from_mode(0o666));
        let temp_file = builder.tempfile_in(parent)?;
        if let Some(permissions) = existing_permissions {
            temp_file.as_file().set_permissions(permissions)?;
        }
        temp_file
    };

    #[cfg(not(unix))]
    let mut temp_file = NamedTempFile::new_in(parent)?;

    if let Err(error) = temp_file.write_all(content.as_bytes()) {
        let _ = temp_file.close();
        return Err(error);
    }
    if let Err(error) = temp_file.as_file().sync_all() {
        let _ = temp_file.close();
        return Err(error);
    }

    let (temp_handle, temp_path) = match temp_file.keep() {
        Ok(parts) => parts,
        Err(error) => {
            let source = error.error;
            let _ = error.file.close();
            return Err(source);
        }
    };
    drop(temp_handle);
    #[cfg(test)]
    let rename_result = crate::provider_resolver::rename_lock_file_for_test(&temp_path, &path);
    #[cfg(not(test))]
    let rename_result = fs::rename(&temp_path, &path);
    if let Err(error) = rename_result {
        let _ = fs::remove_file(temp_path);
        return Err(error);
    }

    sync_parent_directory(parent);
    Ok(())
}

#[cfg(any(test, feature = "test-utils"))]
pub(crate) fn save_lock_file_fixture(lock_file: &LockFile, path: &Path) -> io::Result<()> {
    save_lock_file_to_path(lock_file, path)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io;
    use std::sync::mpsc;
    use std::time::Duration;

    use super::{
        ArtifactTarget, ProviderInstallLock, install_artifact_atomically_with,
        install_project_artifact_atomically, validate_project_target,
    };
    use crate::provider_resolver::WasmArtifactPath;

    #[test]
    fn provider_install_lock_serializes_acquisitions() {
        let project = tempfile::tempdir().unwrap();
        let first = ProviderInstallLock::acquire(project.path()).unwrap();
        let project_dir = project.path().to_path_buf();
        let (started_tx, started_rx) = mpsc::channel();
        let (acquired_tx, acquired_rx) = mpsc::channel();

        let waiter = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            let second = ProviderInstallLock::acquire(&project_dir).unwrap();
            acquired_tx.send(()).unwrap();
            drop(second);
        });

        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(
            acquired_rx
                .recv_timeout(Duration::from_millis(250))
                .is_err(),
            "a second acquisition must remain blocked while the first guard is alive"
        );

        drop(first);
        acquired_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        waiter.join().unwrap();
    }

    #[test]
    fn atomic_installer_replaces_existing_artifact_without_temp_residue() {
        let project = tempfile::tempdir().unwrap();
        let guard = ProviderInstallLock::acquire(project.path()).unwrap();
        let cache_dir = project.path().join(".carina/providers/test/1.0.0");
        fs::create_dir_all(&cache_dir).unwrap();
        let source = project.path().join("replacement.wasm");
        fs::write(&source, b"complete replacement").unwrap();
        let destination = WasmArtifactPath::new(cache_dir.join("test.wasm"), "test").unwrap();
        fs::write(destination.as_path(), b"old artifact").unwrap();

        install_project_artifact_atomically(&guard, &source, &destination).unwrap();

        assert_eq!(
            fs::read(destination.as_path()).unwrap(),
            b"complete replacement"
        );
        let entries = fs::read_dir(&cache_dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        assert_eq!(entries, vec![destination.as_path().to_path_buf()]);
    }

    #[test]
    fn atomic_installer_falls_back_to_staged_copy_when_hard_link_fails() {
        let project = tempfile::tempdir().unwrap();
        let guard = ProviderInstallLock::acquire(project.path()).unwrap();
        let cache_dir = project.path().join(".carina/providers/test/1.0.0");
        fs::create_dir_all(&cache_dir).unwrap();
        let source = project.path().join("source.wasm");
        fs::write(&source, b"provider wasm").unwrap();
        let destination = cache_dir.join("test.wasm");
        let target = ArtifactTarget::Project {
            _install_lock: &guard,
            path: destination.clone(),
        };

        install_artifact_atomically_with(&source, target, |_, _| {
            Err(io::Error::other("injected hard-link failure"))
        })
        .unwrap();

        assert_eq!(fs::read(&destination).unwrap(), b"provider wasm");
        let entries = fs::read_dir(&cache_dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        assert_eq!(entries, vec![destination]);
    }

    #[test]
    fn atomic_installer_reports_hard_link_and_copy_failures() {
        let project = tempfile::tempdir().unwrap();
        let guard = ProviderInstallLock::acquire(project.path()).unwrap();
        let source = project.path().join("source-directory");
        fs::create_dir(&source).unwrap();
        let target = ArtifactTarget::Project {
            _install_lock: &guard,
            path: project
                .path()
                .join(".carina/providers/test/1.0.0/test.wasm"),
        };

        let error = install_artifact_atomically_with(&source, target, |_, _| {
            Err(io::Error::other("injected hard-link failure"))
        })
        .expect_err("copying a directory as an artifact must fail");

        let message = error.to_string();
        assert!(message.contains("injected hard-link failure"), "{message}");
        assert!(message.contains("staged copy fallback failed"), "{message}");
    }

    #[test]
    fn project_target_rejects_parent_components_below_cache_prefix() {
        let project = tempfile::tempdir().unwrap();
        let guard = ProviderInstallLock::acquire(project.path()).unwrap();
        let destination = project
            .path()
            .join(".carina/providers/source/../../outside.wasm");

        let error = validate_project_target(&guard, &destination)
            .expect_err("parent components must not escape the provider cache");

        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }

    #[cfg(unix)]
    #[test]
    fn staged_artifact_uses_umask_adjusted_default_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let project = tempfile::tempdir().unwrap();
        let guard = ProviderInstallLock::acquire(project.path()).unwrap();
        let cache_dir = project.path().join(".carina/providers/test/1.0.0");
        fs::create_dir_all(&cache_dir).unwrap();
        let probe_path = cache_dir.join("probe.wasm");
        fs::write(&probe_path, b"probe").unwrap();
        let destination = WasmArtifactPath::new(cache_dir.join("test.wasm"), "test").unwrap();

        let mut staged = super::StagedArtifact::for_project(&guard, &destination).unwrap();
        std::io::Write::write_all(&mut staged, b"provider wasm").unwrap();
        staged.publish().unwrap();

        let expected_mode = fs::metadata(&probe_path).unwrap().permissions().mode() & 0o777;
        let actual_mode = fs::metadata(destination.as_path())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            actual_mode, expected_mode,
            "a published stage must use fs::write's umask-adjusted mode"
        );
    }
}
