use std::fs::{File, TryLockError};
use std::io::{self, Write};
use std::ops::{Deref, DerefMut};
use std::path::{Component, Path, PathBuf};

use carina_core::UserDirs;
use carina_core::paths::{DisplayPath, ResolvedPath};

use crate::provider_resolver::{
    LockEntry, LockFile, LockFileError, LockFileMigration, RegistrySequenceNotice, WasmArtifactPath,
};
use crate::verified_artifact::{VerifiedArtifact, VerifiedArtifactError, publish_cached};

const PROVIDER_INSTALL_LOCK_FILE_NAME: &str = "provider-install.lock";

/// Exclusive ownership of provider installation and lock-file mutation for one project.
pub struct ProviderInstallLock {
    _guard_file: File,
    project_dir: ResolvedPath,
}

/// Provider lock state loaded, or defaulted, while its project's install lock is held.
///
/// The value borrows the install lock, so it can only be saved while that lock
/// remains held. Its contents can still be replaced through [`DerefMut`].
pub struct GuardedLockFile<'lock> {
    install_lock: &'lock ProviderInstallLock,
    lock_file: LockFile,
    migration: Option<LockFileMigration>,
    pending_registry_sequence_notices: Vec<RegistrySequenceNotice>,
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
    pub fn project_dir(&self) -> &ResolvedPath {
        self.install_lock.project_dir()
    }

    pub(crate) fn install_lock(&self) -> &'lock ProviderInstallLock {
        self.install_lock
    }

    /// Clone the durable state for a transactional save and transfer sole
    /// ownership of any notices awaiting that save.
    pub(crate) fn clone_for_save(&mut self) -> GuardedLockFile<'lock> {
        GuardedLockFile {
            install_lock: self.install_lock,
            lock_file: self.lock_file.clone(),
            migration: None,
            pending_registry_sequence_notices: std::mem::take(
                &mut self.pending_registry_sequence_notices,
            ),
        }
    }

    /// Return notices from a transactional clone whose update or save failed.
    pub(crate) fn restore_registry_sequence_notices_from(
        &mut self,
        failed_save: &mut GuardedLockFile<'lock>,
    ) {
        debug_assert!(self.pending_registry_sequence_notices.is_empty());
        self.pending_registry_sequence_notices
            .append(&mut failed_save.pending_registry_sequence_notices);
    }

    pub fn save(&mut self) -> io::Result<()> {
        save_lock_file_to_path(
            &self.lock_file,
            &self
                .install_lock
                .project_dir()
                .join(crate::provider_resolver::PROVIDER_LOCK_FILE_NAME)?,
        )?;
        for notice in self.pending_registry_sequence_notices.drain(..) {
            eprintln!("{notice}");
        }
        Ok(())
    }

    pub(crate) fn queue_registry_sequence_notice(
        &mut self,
        notice: Option<RegistrySequenceNotice>,
    ) {
        self.pending_registry_sequence_notices.extend(notice);
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
    pub fn acquire(project_dir: &ResolvedPath) -> io::Result<Self> {
        let metadata = project_dir.metadata().map_err(|error| {
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

        let carina_dir = project_dir.join(".carina")?;
        match carina_dir.create_dir() {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        let guard_path = carina_dir.join(PROVIDER_INSTALL_LOCK_FILE_NAME)?;
        let guard_file = guard_path.open_lock()?;

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
            project_dir: project_dir.clone(),
        })
    }

    pub fn project_dir(&self) -> &ResolvedPath {
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
                    pending_registry_sequence_notices: Vec::new(),
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
                pending_registry_sequence_notices: Vec::new(),
            })
        })
    }

    #[cfg(test)]
    pub(crate) fn guard_lock_file_for_test(&self, lock_file: LockFile) -> GuardedLockFile<'_> {
        GuardedLockFile {
            install_lock: self,
            lock_file,
            migration: None,
            pending_registry_sequence_notices: Vec::new(),
        }
    }

    pub(crate) fn remove_project_artifact(&self, artifact: &WasmArtifactPath) -> io::Result<()> {
        validate_project_target(self, artifact.as_path())?;
        project_artifact(self, artifact)?.remove()
    }
}

/// A uniquely named artifact staged beside its final destination.
///
/// The destination is private and publication consumes the stage, so callers can
/// only make bytes visible through the atomic rename implemented here.
pub(crate) struct StagedArtifact<'a> {
    file: carina_core::paths::StagedFile,
    install_lock: &'a ProviderInstallLock,
    destination: WasmArtifactPath,
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
        let target = project_artifact(install_lock, destination)?;
        if let Some(parent) = target.parent() {
            parent.create_dir_all()?;
        }
        Ok(Self {
            file: target.stage()?,
            install_lock,
            destination: destination.clone(),
        })
    }
    fn from_source_for_project(
        install_lock: &'a ProviderInstallLock,
        source: &ResolvedPath,
        destination: &WasmArtifactPath,
    ) -> io::Result<Self> {
        let mut staged = Self::for_project(install_lock, destination)?;
        staged.write_all(&source.read()?)?;
        Ok(staged)
    }
    pub(crate) fn read_bytes(&mut self) -> io::Result<Vec<u8>> {
        use std::io::{Read, Seek};
        self.file.rewind()?;
        let mut bytes = Vec::new();
        self.file.read_to_end(&mut bytes)?;
        Ok(bytes)
    }
    pub(crate) fn destination(&self) -> &WasmArtifactPath {
        &self.destination
    }
    pub(crate) fn project_dir(&self) -> &ResolvedPath {
        self.install_lock.project_dir()
    }
    pub(crate) fn publish(self) -> io::Result<()> {
        self.file.publish()
    }
}
impl Write for StagedArtifact<'_> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.file.write(buffer)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

pub(crate) fn install_project_artifact_atomically(
    install_lock: &ProviderInstallLock,
    source: &ResolvedPath,
    destination: &WasmArtifactPath,
) -> io::Result<String> {
    use sha2::{Digest, Sha256};
    let bytes = source.read()?;
    let mut staged = StagedArtifact::for_project(install_lock, destination)?;
    staged.write_all(&bytes)?;
    staged.publish()?;
    Ok(format!("{:x}", Sha256::digest(&bytes)))
}

pub(crate) fn install_project_artifact_from_global_cache<R>(
    install_lock: &ProviderInstallLock,
    source: &ResolvedPath,
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
    source: &ResolvedPath,
    destination: &GlobalArtifactPath,
) -> io::Result<()> {
    let target = ResolvedPath::new(destination.as_path())?;
    if let Some(parent) = target.parent() {
        parent.create_dir_all()?;
    }
    target.atomic_replace(&source.read()?)
}

fn validate_project_target(
    install_lock: &ProviderInstallLock,
    destination: &Path,
) -> io::Result<()> {
    let cache = install_lock
        .project_dir()
        .display()
        .join(".carina/providers");
    let label = DisplayPath::new(destination)?;
    if label.relative_to(&cache).is_some_and(|relative| {
        !Path::new(&relative).components().any(|part| {
            matches!(
                part,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    }) {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("project provider artifact {label} is outside {cache}"),
        ))
    }
}

fn project_artifact(
    install_lock: &ProviderInstallLock,
    artifact: &WasmArtifactPath,
) -> io::Result<ResolvedPath> {
    validate_project_target(install_lock, artifact.as_path())?;
    install_lock.project_dir().join(artifact.as_path())
}

fn locate_lock_file_error(location: &DisplayPath, operation: &str, cause: io::Error) -> io::Error {
    io::Error::new(
        cause.kind(),
        format!("Failed to {operation} provider lock file at {location}: {cause}"),
    )
}

fn save_lock_file_to_path(lock_file: &LockFile, path: &ResolvedPath) -> io::Result<()> {
    let content = lock_file
        .to_toml_string()
        .map_err(|error| io::Error::other(format!("Failed to serialize lock file: {error}")))?;
    let mut staged = path
        .stage()
        .map_err(|error| locate_lock_file_error(path.display(), "stage", error))?;
    staged
        .write_all(content.as_bytes())
        .map_err(|error| locate_lock_file_error(path.display(), "write staged", error))?;
    staged
        .sync_all()
        .map_err(|error| locate_lock_file_error(path.display(), "sync staged", error))?;
    #[cfg(test)]
    if let Some(result) = crate::provider_resolver::replace_lock_file_for_test(&mut staged, path) {
        result.map_err(|error| locate_lock_file_error(path.display(), "replace", error))?;
    }
    staged
        .publish()
        .map_err(|error| locate_lock_file_error(path.display(), "replace", error))
}

#[cfg(any(test, feature = "test-utils"))]
pub(crate) fn save_lock_file_fixture(lock_file: &LockFile, path: &Path) -> io::Result<()> {
    let display = DisplayPath::new(path)?;
    let target = ResolvedPath::new(path).map_err(|error| {
        locate_lock_file_error(&display, "resolve replacement target for", error)
    })?;
    save_lock_file_to_path(lock_file, &target)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io;
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn lock_file_error_preserves_operation_and_internal_detail() {
        let temp = tempfile::tempdir().unwrap();
        let location =
            carina_core::paths::DisplayPath::new(temp.path().join("alias.lock")).unwrap();
        for (operation, detail) in [
            (
                "resolve replacement target for",
                "too many symbolic links in replacement target",
            ),
            ("write staged", "injected staged write failure"),
            ("sync staged", "injected staged sync failure"),
            ("replace", "injected lock-file rename failure"),
        ] {
            let error =
                super::locate_lock_file_error(&location, operation, io::Error::other(detail));
            assert_eq!(error.kind(), io::ErrorKind::Other);
            assert_eq!(
                error.to_string(),
                format!("Failed to {operation} provider lock file at {location}: {detail}"),
            );
        }
    }

    #[test]
    fn lock_file_replacement_error_uses_display_path_and_preserves_os_error() {
        let temp = tempfile::tempdir().unwrap();
        let target =
            carina_core::paths::ResolvedPath::new(temp.path().join("missing/alias.lock")).unwrap();
        let error =
            super::save_lock_file_to_path(&super::LockFile::default(), &target).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert!(error.to_string().contains(&target.display().to_string()));
        assert!(error.to_string().contains("stage provider lock file"));
    }

    #[cfg(unix)]
    #[test]
    fn lock_file_save_error_preserves_symlink_location_before_staging() {
        let temp = tempfile::tempdir().unwrap();
        let real = temp.path().join("real");
        let link = temp.path().join("link");
        fs::create_dir(&real).unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();
        fs::write(real.join("blocked"), b"not a directory").unwrap();
        let path = link.join("blocked/carina-providers.lock");
        let error = super::LockFile::default().save(&path).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotADirectory);
        let message = error.to_string();
        assert!(
            message.starts_with(&format!(
                "Failed to resolve replacement target for provider lock file at {}:",
                path.display()
            )),
            "{message}",
        );
        assert!(message.contains(&path.display().to_string()), "{message}");
        assert!(!message.contains(&real.display().to_string()), "{message}");
    }

    #[cfg(unix)]
    #[test]
    fn lock_file_staging_error_preserves_symlink_location() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let temp = tempfile::tempdir().unwrap();
        let real = temp.path().join("real");
        let link = temp.path().join("link");
        fs::create_dir(&real).unwrap();
        symlink(&real, &link).unwrap();
        fs::write(real.join("target.lock"), "").unwrap();
        let alias = link.join("carina-providers.lock");
        symlink("target.lock", &alias).unwrap();
        let permissions = fs::metadata(&real).unwrap().permissions();
        fs::set_permissions(&real, fs::Permissions::from_mode(0o555)).unwrap();
        // Privileged processes can bypass directory permissions.
        if fs::write(real.join("write-probe"), "").is_ok() {
            fs::set_permissions(&real, permissions).unwrap();
            return;
        }
        let result = super::LockFile::default().save(&alias);
        fs::set_permissions(&real, permissions).unwrap();
        let error = result.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        let display = error.to_string();
        let debug = format!("{error:?}");
        assert!(
            display.starts_with(&format!(
                "Failed to stage provider lock file at {}:",
                alias.display()
            )),
            "{display}"
        );
        assert!(display.contains(&alias.display().to_string()), "{display}");
        assert!(!display.contains(&real.display().to_string()), "{display}");
        assert!(!debug.contains(&real.display().to_string()), "{debug}");
        assert_eq!(fs::read_to_string(real.join("target.lock")).unwrap(), "");
    }

    use super::{
        ProviderInstallLock, install_project_artifact_atomically, validate_project_target,
    };
    use crate::provider_resolver::WasmArtifactPath;

    #[test]
    fn provider_install_lock_serializes_acquisitions() {
        let project = tempfile::tempdir().unwrap();
        let first = ProviderInstallLock::acquire(
            &carina_core::paths::ResolvedPath::new(project.path()).unwrap(),
        )
        .unwrap();
        let project_dir = project.path().to_path_buf();
        let (started_tx, started_rx) = mpsc::channel();
        let (acquired_tx, acquired_rx) = mpsc::channel();

        let waiter = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            let second = ProviderInstallLock::acquire(
                &carina_core::paths::ResolvedPath::new(&project_dir).unwrap(),
            )
            .unwrap();
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
        let guard = ProviderInstallLock::acquire(
            &carina_core::paths::ResolvedPath::new(project.path()).unwrap(),
        )
        .unwrap();
        let cache_dir = project.path().join(".carina/providers/test/1.0.0");
        fs::create_dir_all(&cache_dir).unwrap();
        let source = project.path().join("replacement.wasm");
        fs::write(&source, b"complete replacement").unwrap();
        let destination = WasmArtifactPath::new(cache_dir.join("test.wasm"), "test").unwrap();
        fs::write(destination.as_path(), b"old artifact").unwrap();

        install_project_artifact_atomically(
            &guard,
            &carina_core::paths::ResolvedPath::new(&source).unwrap(),
            &destination,
        )
        .unwrap();

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
    fn atomic_installer_read_failure_preserves_existing_artifact() {
        let project = tempfile::tempdir().unwrap();
        let root = carina_core::paths::ResolvedPath::new(project.path()).unwrap();
        let guard = ProviderInstallLock::acquire(&root).unwrap();
        let cache = root.join(".carina/providers/test/1.0.0").unwrap();
        cache.create_dir_all().unwrap();
        let destination = WasmArtifactPath::new(
            project
                .path()
                .join(".carina/providers/test/1.0.0/test.wasm"),
            "test",
        )
        .unwrap();
        cache
            .join("test.wasm")
            .unwrap()
            .atomic_replace(b"old artifact")
            .unwrap();
        let source = root.join("missing.wasm").unwrap();
        let error = install_project_artifact_atomically(&guard, &source, &destination).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert_eq!(
            cache.join("test.wasm").unwrap().read().unwrap(),
            b"old artifact"
        );
        assert_eq!(cache.entries().unwrap().len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn provider_install_lock_symlink_retarget_pins_lock_and_artifact_writes() {
        let temp = tempfile::tempdir().unwrap();
        let original = temp.path().join("original");
        let replacement = temp.path().join("replacement");
        let alias = temp.path().join("alias");
        fs::create_dir(&original).unwrap();
        fs::create_dir(&replacement).unwrap();
        fs::write(original.join("source.wasm"), b"original provider").unwrap();
        fs::write(replacement.join("source.wasm"), b"replacement provider").unwrap();
        std::os::unix::fs::symlink(&original, &alias).unwrap();
        let root = carina_core::paths::ResolvedPath::new(&alias).unwrap();
        fs::remove_file(&alias).unwrap();
        std::os::unix::fs::symlink(&replacement, &alias).unwrap();
        let guard = ProviderInstallLock::acquire(&root).unwrap();
        let source = root.join("source.wasm").unwrap();
        let destination = WasmArtifactPath::new(
            alias.join(".carina/providers/file/source/source.wasm"),
            "test",
        )
        .unwrap();
        install_project_artifact_atomically(&guard, &source, &destination).unwrap();
        guard.load_lock_file_or_default().unwrap().save().unwrap();
        assert!(original.join(".carina/provider-install.lock").exists());
        assert!(original.join("carina-providers.lock").exists());
        assert_eq!(
            fs::read(original.join(".carina/providers/file/source/source.wasm")).unwrap(),
            b"original provider"
        );
        assert!(!replacement.join(".carina").exists());
        assert!(!replacement.join("carina-providers.lock").exists());
    }

    #[test]
    fn project_target_rejects_parent_components_below_cache_prefix() {
        let project = tempfile::tempdir().unwrap();
        let guard = ProviderInstallLock::acquire(
            &carina_core::paths::ResolvedPath::new(project.path()).unwrap(),
        )
        .unwrap();
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
        let guard = ProviderInstallLock::acquire(
            &carina_core::paths::ResolvedPath::new(project.path()).unwrap(),
        )
        .unwrap();
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
