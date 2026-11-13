//! Typed selection of the state backend an operation is allowed to open.
//!
//! Raw backend construction is crate-private. Public callers obtain only an
//! operation-specific capability through [`BackendSelection`], keeping
//! lock-aware reads, verified mutations, migrations, force-unlock, and
//! state-bucket deletion distinct at the call site.

use carina_core::paths::ResolvedPath;
use std::path::PathBuf;

use carina_core::parser::BackendConfig as ParserBackendConfig;
use carina_core::resource::{
    AbsoluteBindingAddress, BindingAddress, NoPlaceholder, PlaceholderKind,
};
use thiserror::Error;

use crate::backend::{BackendError, BackendResult};
use crate::backend_config::{BackendConfig, S3BackendSettings};
use crate::backend_lock::BackendLock;
use crate::backends::{S3_BACKEND_PROVIDER_NAME, S3_BACKEND_RESOURCE_TYPE, StateUrl};

/// The old and new backend addresses involved in a pending migration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendDrift {
    project_dir: ResolvedPath,
    existing: BackendLock,
    configured: BackendLock,
}

impl BackendDrift {
    pub fn existing(&self) -> &BackendLock {
        &self.existing
    }

    pub fn configured(&self) -> &BackendLock {
        &self.configured
    }
}

/// Comparison between the configured backend and `carina-backend.lock`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendDriftStatus {
    Fresh,
    Unchanged,
    Drifted(Box<BackendDrift>),
}

/// Select operation-specific capabilities for project or URL-backed state.
#[derive(Debug, Clone)]
pub struct BackendSelection {
    _private: (),
}

/// A project backend selected from admitted settings and its source directory.
#[derive(Debug, Clone)]
pub(crate) struct ConfiguredBackend {
    origin: AdmittedBackendOrigin,
    base_dir: ResolvedPath,
}

/// A backend candidate together with the source that selected it.
/// An implicit source always denotes the admitted local default.
#[derive(Debug)]
pub enum BackendConfigurationSource<
    'a,
    P: PlaceholderKind = NoPlaceholder,
    B: BindingAddress = AbsoluteBindingAddress,
> {
    Implicit,
    Declared(&'a ParserBackendConfig<P, B>),
}
impl<P: PlaceholderKind, B: BindingAddress> Copy for BackendConfigurationSource<'_, P, B> {}
impl<P: PlaceholderKind, B: BindingAddress> Clone for BackendConfigurationSource<'_, P, B> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<'a, P: PlaceholderKind, B: BindingAddress> From<Option<&'a ParserBackendConfig<P, B>>>
    for BackendConfigurationSource<'a, P, B>
{
    fn from(config: Option<&'a ParserBackendConfig<P, B>>) -> Self {
        match config {
            Some(config) => Self::Declared(config),
            None => Self::Implicit,
        }
    }
}
impl<P: PlaceholderKind, B: BindingAddress> BackendConfigurationSource<'_, P, B> {
    fn admit(self) -> BackendResult<AdmittedBackendOrigin> {
        match self {
            Self::Implicit => Ok(AdmittedBackendOrigin::implicit()),
            Self::Declared(config) => Ok(AdmittedBackendOrigin::Declared(BackendConfig::try_from(
                config,
            )?)),
        }
    }
    pub fn backend_lock(self) -> BackendResult<BackendLock> {
        Ok(BackendLock::from_config(self.admit()?.config()))
    }
}

#[derive(Debug, Clone)]
enum AdmittedBackendOrigin {
    Implicit(BackendConfig),
    Declared(BackendConfig),
    Locked(BackendConfig),
}
impl AdmittedBackendOrigin {
    fn implicit() -> Self {
        Self::Implicit(BackendConfig::Local(BackendConfig::implicit_local()))
    }
    fn config(&self) -> &BackendConfig {
        match self {
            Self::Implicit(config) | Self::Declared(config) | Self::Locked(config) => config,
        }
    }
    fn kind(&self) -> ReadBackendOrigin {
        match self {
            Self::Implicit(_) => ReadBackendOrigin::Implicit,
            Self::Declared(_) => ReadBackendOrigin::Declared,
            Self::Locked(_) => ReadBackendOrigin::Locked,
        }
    }
}

/// Source selected by the lock-aware read owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadBackendOrigin {
    Implicit,
    Declared,
    Locked,
}

/// Lock-aware selection for a read-only operation.
#[derive(Debug, Clone)]
pub struct ReadBackendSelection {
    configured: ConfiguredBackend,
    lock_present: bool,
    drift: Option<BackendDrift>,
}

impl ReadBackendSelection {
    pub fn lock_present(&self) -> bool {
        self.lock_present
    }

    /// Construct the selected read capability, invoking `warn` on pending
    /// migration drift before that capability becomes available to the caller.
    ///
    /// The underlying [`BackendSelection`] is intentionally not exposed: a
    /// project-backed reader must cross this warning acknowledgement seam.
    pub async fn resolve_with_drift_warning(
        self,
        warn: impl FnOnce(&BackendDrift),
    ) -> BackendResult<ResolvedReadBackend> {
        let selected_config = self.configured.config().clone();
        let migration_pending = self.drift.is_some();
        let backend = crate::backends::create_backend(&self.configured).await?;
        if let Some(drift) = self.drift.as_ref() {
            warn(drift);
        }
        Ok(ResolvedReadBackend {
            backend,
            selected_config,
            origin: self.configured.origin(),
            migration_pending,
        })
    }
}

/// A backend narrowed to the operations used by read-only commands.
pub struct ResolvedReadBackend {
    backend: Box<dyn crate::StateBackend>,
    selected_config: BackendConfig,
    origin: ReadBackendOrigin,
    migration_pending: bool,
}

impl ResolvedReadBackend {
    pub async fn read_state(&self) -> BackendResult<Option<crate::LoadedState>> {
        self.backend.read_state().await
    }

    pub async fn read_published_exports(&self) -> BackendResult<Option<crate::PublishedExports>> {
        crate::backend::read_published_exports(self.backend.as_ref()).await
    }

    pub async fn bucket_exists(&self) -> BackendResult<bool> {
        self.backend.bucket_exists().await
    }

    pub fn provider_name(&self) -> Option<&str> {
        self.backend.provider_name()
    }

    pub fn resource_type(&self) -> Option<&str> {
        self.backend.resource_type()
    }

    pub fn selected_config(&self) -> &BackendConfig {
        &self.selected_config
    }

    pub fn origin(&self) -> ReadBackendOrigin {
        self.origin
    }

    pub fn migration_pending(&self) -> bool {
        self.migration_pending
    }
}

/// Selection produced only after the backend lock exists and matches the
/// configured backend.
#[derive(Debug, Clone)]
pub struct VerifiedMutationBackendSelection {
    configured: ConfiguredBackend,
}

impl VerifiedMutationBackendSelection {
    pub fn s3_settings(&self) -> Option<&S3BackendSettings> {
        self.configured.config().as_s3()
    }

    pub async fn resolve(&self) -> BackendResult<Box<dyn crate::StateBackend>> {
        crate::backends::create_backend(&self.configured).await
    }
}

/// Exhaustive result of attempting to select a mutation backend.
#[derive(Debug)]
pub enum MutationBackendSelection {
    Verified(VerifiedMutationBackendSelection),
    MissingLock { path: PathBuf },
    Drifted(BackendDrift),
}

/// Backends resolved for one state migration.
///
/// Only the operations required by migration are exposed: read the source,
/// inspect/write/verify the target, and use the same pinned local source for
/// post-commit cleanup. The source locator is never resolved a second time.
pub struct MigrationBackends {
    source: Box<dyn crate::StateBackend>,
    target: Box<dyn crate::StateBackend>,
    local_source_path: Option<ResolvedPath>,
}

impl MigrationBackends {
    pub async fn read_source_state(&self) -> BackendResult<Option<crate::LoadedState>> {
        self.source.read_state().await
    }

    pub async fn read_target_state(&self) -> BackendResult<Option<crate::LoadedState>> {
        self.target.read_state().await
    }

    pub async fn write_target_state(&self, state: &crate::StateFile) -> BackendResult<()> {
        self.target.write_state(state).await
    }

    pub fn local_source_path(&self) -> Option<&ResolvedPath> {
        self.local_source_path.as_ref()
    }
}

/// Selection proving that a requested state bucket is the configured S3
/// backend bucket.
#[derive(Debug, Clone)]
pub struct BucketDeleteBackendSelection {
    _private: (),
}

/// The requested bucket does not match the configured state bucket.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{message}")]
pub struct BucketDeleteSelectionError {
    message: String,
}

impl BucketDeleteSelectionError {
    pub fn into_message(self) -> String {
        self.message
    }
}

impl BucketDeleteBackendSelection {
    pub fn provider_name(&self) -> &'static str {
        S3_BACKEND_PROVIDER_NAME
    }

    pub fn resource_type(&self) -> &'static str {
        S3_BACKEND_RESOURCE_TYPE
    }
}

/// Selection for a user-supplied `--state-url`.
#[derive(Debug, Clone)]
pub(crate) struct ExplicitUrlBackendSelection {
    url: StateUrl,
}

impl ExplicitUrlBackendSelection {
    pub(crate) async fn read_state(&self) -> BackendResult<Option<crate::LoadedState>> {
        crate::backends::create_explicit_backend(&self.url)
            .await?
            .read_state()
            .await
    }

    pub(crate) async fn read_published_exports(
        &self,
    ) -> BackendResult<Option<crate::PublishedExports>> {
        let backend = crate::backends::create_explicit_backend(&self.url).await?;
        crate::backend::read_published_exports(backend.as_ref()).await
    }
}

struct BackendInspection {
    project_dir: ResolvedPath,
    configured: AdmittedBackendOrigin,
    status: BackendDriftStatus,
}

impl BackendSelection {
    /// Select a backend for a project-backed read.
    ///
    /// A pending migration selects the old backend recorded in the lock and
    /// returns its drift metadata without printing. Missing locks are allowed
    /// here so the command layer can retain command-specific policy.
    pub fn for_read<'a, P: PlaceholderKind + 'a, B: BindingAddress + 'a>(
        project_dir: &ResolvedPath,
        configured: impl Into<BackendConfigurationSource<'a, P, B>>,
    ) -> BackendResult<ReadBackendSelection> {
        let inspection = inspect(project_dir, configured.into())?;
        Self::for_read_inspection(inspection)
    }

    fn for_read_inspection(inspection: BackendInspection) -> BackendResult<ReadBackendSelection> {
        let lock_present = !matches!(inspection.status, BackendDriftStatus::Fresh);
        let (configured, drift) = match inspection.status {
            BackendDriftStatus::Drifted(drift) => {
                let config = drift.existing.to_state_config()?;
                (
                    ConfiguredBackend::new(
                        AdmittedBackendOrigin::Locked(config),
                        inspection.project_dir,
                    ),
                    Some(*drift),
                )
            }
            BackendDriftStatus::Fresh | BackendDriftStatus::Unchanged => (
                ConfiguredBackend::new(inspection.configured, inspection.project_dir),
                None,
            ),
        };
        Ok(ReadBackendSelection {
            configured,
            lock_present,
            drift,
        })
    }

    /// Select a backend for an operation that may mutate state.
    ///
    /// Only the `Verified` result carries a constructible backend token.
    pub fn verified_for_mutation<'a, P: PlaceholderKind + 'a, B: BindingAddress + 'a>(
        project_dir: &ResolvedPath,
        configured: impl Into<BackendConfigurationSource<'a, P, B>>,
    ) -> BackendResult<MutationBackendSelection> {
        let inspection = inspect(project_dir, configured.into())?;
        Self::verified_for_mutation_inspection(inspection)
    }

    fn verified_for_mutation_inspection(
        inspection: BackendInspection,
    ) -> BackendResult<MutationBackendSelection> {
        match inspection.status {
            BackendDriftStatus::Fresh => Ok(MutationBackendSelection::MissingLock {
                path: std::path::PathBuf::from(
                    inspection
                        .project_dir
                        .display()
                        .join(crate::backend_lock::LOCK_FILE)
                        .to_string(),
                ),
            }),
            BackendDriftStatus::Unchanged => Ok(MutationBackendSelection::Verified(
                VerifiedMutationBackendSelection {
                    configured: ConfiguredBackend::new(
                        inspection.configured,
                        inspection.project_dir,
                    ),
                },
            )),
            BackendDriftStatus::Drifted(drift) => Ok(MutationBackendSelection::Drifted(*drift)),
        }
    }

    /// Resolve the old and new backend addresses carried by a proven drift.
    pub async fn for_migration(drift: &BackendDrift) -> BackendResult<MigrationBackends> {
        let (source, local_source_path) =
            resolve_migration_backend(&drift.existing.to_state_config()?, &drift.project_dir)
                .await?;
        let (target, local_target_path) =
            resolve_migration_backend(&drift.configured.to_state_config()?, &drift.project_dir)
                .await?;
        if let (Some(source), Some(target)) = (&local_source_path, &local_target_path)
            && source.identity() == target.identity()
        {
            return Err(BackendError::Configuration(format!(
                "The locked backend {} and configured backend {} resolve to the same local state file; \
                 migration would remove its own destination. Run init without --migrate-state to adopt the new address.",
                source.display(),
                target.display(),
            )));
        }
        Ok(MigrationBackends {
            source,
            target,
            local_source_path,
        })
    }

    /// Select the configured S3 backend only when the requested bucket is the
    /// same bucket the command is authorized to delete.
    pub fn for_bucket_delete(
        settings: &S3BackendSettings,
        requested_bucket: &str,
    ) -> Result<BucketDeleteBackendSelection, BucketDeleteSelectionError> {
        if settings.bucket() != requested_bucket {
            let configured_bucket = match settings.bucket_diagnostic_literal() {
                Some(bucket) => format!("'{bucket}'"),
                None => "value from env()".to_string(),
            };
            return Err(BucketDeleteSelectionError {
                message: format!(
                    "Bucket name '{requested_bucket}' does not match backend configuration bucket \
                     {configured_bucket}."
                ),
            });
        }

        Ok(BucketDeleteBackendSelection { _private: () })
    }

    /// Select the exact URL supplied by the user, bypassing project backend
    /// configuration and its lock by explicit contract.
    pub(crate) fn explicit_url(url: &StateUrl) -> ExplicitUrlBackendSelection {
        ExplicitUrlBackendSelection { url: url.clone() }
    }

    /// Force-release a backend lock, selecting the old locked backend during
    /// pending migration without exposing a general backend capability.
    pub async fn force_unlock<P: PlaceholderKind, B: BindingAddress>(
        project_dir: &ResolvedPath,
        configured: Option<&ParserBackendConfig<P, B>>,
        lock_id: &str,
    ) -> BackendResult<()> {
        let selected = Self::for_read(project_dir, configured)?;
        crate::backends::create_backend(&selected.configured)
            .await?
            .force_unlock(lock_id)
            .await
    }
}

impl ConfiguredBackend {
    fn new(origin: AdmittedBackendOrigin, base_dir: ResolvedPath) -> Self {
        Self { origin, base_dir }
    }
    pub(crate) fn origin(&self) -> ReadBackendOrigin {
        self.origin.kind()
    }
    pub(crate) fn config(&self) -> &BackendConfig {
        self.origin.config()
    }
    pub(crate) async fn open(&self) -> BackendResult<Box<dyn crate::StateBackend>> {
        match self.config() {
            BackendConfig::S3(settings) => Ok(Box::new(
                crate::backends::S3Backend::from_config(settings).await?,
            )),
            BackendConfig::Local(settings) => {
                let backend = match self.origin {
                    AdmittedBackendOrigin::Implicit(_) => {
                        crate::backends::LocalBackend::from_location(
                            settings.pin_address(&self.base_dir)?,
                        )?
                    }
                    AdmittedBackendOrigin::Declared(_) | AdmittedBackendOrigin::Locked(_) => {
                        crate::backends::LocalBackend::from_config(settings, &self.base_dir)?
                    }
                };
                Ok(Box::new(backend))
            }
        }
    }
}

/// Migration's alias check and IO consume the same pin, regardless of origin.
async fn resolve_migration_backend(
    config: &BackendConfig,
    base: &ResolvedPath,
) -> BackendResult<(Box<dyn crate::StateBackend>, Option<ResolvedPath>)> {
    match config {
        BackendConfig::Local(settings) => {
            let location = settings.pin_address(base)?;
            let path = location.path().clone();
            let backend = crate::backends::LocalBackend::from_location(location)?;
            Ok((Box::new(backend), Some(path)))
        }
        BackendConfig::S3(settings) => Ok((
            Box::new(crate::backends::S3Backend::from_config(settings).await?),
            None,
        )),
    }
}

/// Inspect backend drift without constructing a backend.
pub fn inspect_backend_drift<P: PlaceholderKind, B: BindingAddress>(
    project_dir: &ResolvedPath,
    configured: Option<&ParserBackendConfig<P, B>>,
) -> BackendResult<BackendDriftStatus> {
    inspect(project_dir, configured.into()).map(|inspection| inspection.status)
}

fn inspect<P: PlaceholderKind, B: BindingAddress>(
    project_dir: &ResolvedPath,
    configured: BackendConfigurationSource<'_, P, B>,
) -> BackendResult<BackendInspection> {
    inspect_resolved(project_dir, configured.admit()?)
}

fn inspect_resolved(
    project_dir: &ResolvedPath,
    configured: AdmittedBackendOrigin,
) -> BackendResult<BackendInspection> {
    let project_dir = project_dir.clone();
    let configured_lock = BackendLock::from_config(configured.config());
    let existing = BackendLock::load(&project_dir)?;
    let status = match existing {
        None => BackendDriftStatus::Fresh,
        Some(existing) if existing == configured_lock => BackendDriftStatus::Unchanged,
        Some(existing) => BackendDriftStatus::Drifted(Box::new(BackendDrift {
            project_dir: project_dir.clone(),
            existing,
            configured: configured_lock,
        })),
    };
    Ok(BackendInspection {
        project_dir,
        configured,
        status,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use carina_core::resource::{ConcreteValue, Value};

    use super::*;

    fn local_config(path: &str) -> ParserBackendConfig {
        ParserBackendConfig {
            backend_type: "local".to_string(),
            attributes: (HashMap::<String, _>::from([(
                "path".to_string(),
                Value::Concrete(ConcreteValue::String(path.to_string().into())),
            )]))
            .into(),
            attribute_spans: Default::default(),
        }
    }

    fn s3_config(bucket: &str) -> ParserBackendConfig {
        ParserBackendConfig {
            backend_type: "s3".to_string(),
            attributes: (HashMap::<String, _>::from([
                (
                    "bucket".to_string(),
                    Value::Concrete(ConcreteValue::String(bucket.to_string().into())),
                ),
                (
                    "key".to_string(),
                    Value::Concrete(ConcreteValue::String("state.json".into())),
                ),
                (
                    "region".to_string(),
                    Value::Concrete(ConcreteValue::String("us-east-1".into())),
                ),
            ]))
            .into(),
            attribute_spans: Default::default(),
        }
    }

    #[tokio::test]
    async fn resolved_read_carries_locked_config_and_drift() {
        let project = tempfile::tempdir().unwrap();
        BackendLock::for_config(Some(&local_config("old.json")))
            .unwrap()
            .save(&carina_core::paths::ResolvedPath::new(project.path()).unwrap())
            .unwrap();

        let selected = BackendSelection::for_read(
            &carina_core::paths::ResolvedPath::new(project.path()).unwrap(),
            Some(&local_config("new.json")),
        )
        .unwrap();
        let resolved = selected.resolve_with_drift_warning(|_| {}).await.unwrap();

        let BackendConfig::Local(settings) = resolved.selected_config() else {
            panic!("read must select the old local backend")
        };
        assert_eq!(settings.path(), Some("old.json"));
        assert!(resolved.migration_pending());
    }

    #[tokio::test]
    async fn drifted_read_resolves_only_through_warning_callback() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let project = tempfile::tempdir().unwrap();
        BackendLock::for_config(Some(&local_config("old.json")))
            .unwrap()
            .save(&carina_core::paths::ResolvedPath::new(project.path()).unwrap())
            .unwrap();
        let selected = BackendSelection::for_read(
            &carina_core::paths::ResolvedPath::new(project.path()).unwrap(),
            Some(&local_config("new.json")),
        )
        .unwrap();
        let warned = AtomicBool::new(false);

        let _backend = selected
            .resolve_with_drift_warning(|drift| {
                assert_eq!(
                    drift.existing(),
                    &BackendLock::for_config(Some(&local_config("old.json"))).unwrap()
                );
                warned.store(true, Ordering::SeqCst);
            })
            .await
            .unwrap();

        assert!(warned.load(Ordering::SeqCst));
    }

    #[test]
    fn mutation_selection_is_not_constructible_during_drift() {
        let project = tempfile::tempdir().unwrap();
        BackendLock::for_config(Some(&local_config("old.json")))
            .unwrap()
            .save(&carina_core::paths::ResolvedPath::new(project.path()).unwrap())
            .unwrap();

        let selected = BackendSelection::verified_for_mutation(
            &carina_core::paths::ResolvedPath::new(project.path()).unwrap(),
            Some(&local_config("new.json")),
        )
        .unwrap();

        assert!(matches!(selected, MutationBackendSelection::Drifted(_)));
    }

    #[tokio::test]
    async fn migration_selection_resolves_the_proven_old_and_new_backends() {
        use crate::StateBackend;
        use crate::backends::LocalBackend;
        use crate::state::StateFile;

        let project = tempfile::tempdir().unwrap();
        let old_path = project.path().join("old.json");
        let new_path = project.path().join("new.json");
        let mut source_state = StateFile::new();
        source_state.lineage = "locked-old".to_string();
        LocalBackend::with_path(old_path)
            .write_state(&source_state)
            .await
            .unwrap();
        BackendLock::for_config(Some(&local_config("old.json")))
            .unwrap()
            .save(&carina_core::paths::ResolvedPath::new(project.path()).unwrap())
            .unwrap();
        let BackendDriftStatus::Drifted(drift) = inspect_backend_drift(
            &carina_core::paths::ResolvedPath::new(project.path()).unwrap(),
            Some(&local_config("new.json")),
        )
        .unwrap() else {
            panic!("fixture must have pending backend migration")
        };

        let backends = BackendSelection::for_migration(&drift).await.unwrap();

        let Some(source) = backends.local_source_path() else {
            panic!("migration source must be the locked local backend")
        };
        assert_eq!(
            source.display(),
            &carina_core::paths::DisplayPath::new(project.path().join("old.json")).unwrap()
        );
        let loaded_source = backends.read_source_state().await.unwrap().unwrap();
        assert_eq!(loaded_source.into_state().lineage, "locked-old");
        assert!(backends.read_target_state().await.unwrap().is_none());

        let mut migrated = StateFile::new();
        migrated.lineage = "configured-new".to_string();
        backends.write_target_state(&migrated).await.unwrap();
        let loaded_target = LocalBackend::with_path(new_path)
            .read_state()
            .await
            .unwrap()
            .unwrap();
        assert_eq!(loaded_target.into_state().lineage, "configured-new");
    }

    #[test]
    fn bucket_delete_selection_requires_the_requested_configured_bucket() {
        let config = BackendConfig::try_from(&s3_config("configured-bucket")).unwrap();
        let settings = config.as_s3().unwrap();

        let selected = BackendSelection::for_bucket_delete(settings, "configured-bucket").unwrap();
        assert_eq!(selected.provider_name(), "aws");
        assert_eq!(selected.resource_type(), "s3.Bucket");

        let error = BackendSelection::for_bucket_delete(settings, "different-bucket")
            .expect_err("a bucket-delete token must prove the CLI target matches config");
        assert!(
            error
                .to_string()
                .contains("does not match backend configuration bucket 'configured-bucket'"),
            "{error}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn read_selection_preserves_symlink_backend_base_directory() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let alias = root.path().join("project-alias");
        symlink(&project, &alias).unwrap();

        let selected = BackendSelection::for_read(
            &carina_core::paths::ResolvedPath::new(&alias).unwrap(),
            None::<&ParserBackendConfig>,
        )
        .unwrap();
        let base_dir = &selected.configured.base_dir;
        assert_eq!(base_dir.display().to_string(), alias.display().to_string());
    }
}

#[cfg(test)]
mod default_admission_cleanup_tests {
    use super::*;
    #[tokio::test]
    async fn implicit_local_is_admitted_when_selected_and_retains_its_migration_pin() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("values.crn"), "let input = 'value'\n").unwrap();
        let selected = ConfiguredBackend::new(
            AdmittedBackendOrigin::implicit(),
            ResolvedPath::new(root.path()).unwrap(),
        );
        assert!(
            matches!(selected.config(), BackendConfig::Local(_)),
            "implicit selection must contain admitted local settings"
        );
        let (_, path) = resolve_migration_backend(selected.config(), &selected.base_dir)
            .await
            .unwrap();
        assert_eq!(
            path.unwrap().identity(),
            ResolvedPath::new(root.path().join("carina.state.json"))
                .unwrap()
                .identity()
        );
    }
}

#[cfg(test)]
mod review_r1_selection_tests {
    use super::*;
    fn raw(path: &str) -> ParserBackendConfig {
        ParserBackendConfig {
            backend_type: "local".into(),
            attributes: [(
                "path".to_owned(),
                carina_core::resource::Value::Concrete(
                    carina_core::resource::ConcreteValue::String(path.into()),
                ),
            )]
            .into_iter()
            .collect(),
            attribute_spans: Default::default(),
        }
    }
    #[test]
    fn review_r1_project_capabilities_hold_only_configured_backends() {
        fn accepts_configured(_: &ConfiguredBackend) {}
        let root = tempfile::tempdir().unwrap();
        let base = ResolvedPath::new(root.path()).unwrap();
        let read = BackendSelection::for_read(&base, None::<&ParserBackendConfig>).unwrap();
        accepts_configured(&read.configured);
        BackendLock::for_config(None::<&ParserBackendConfig>)
            .unwrap()
            .save(&base)
            .unwrap();
        let MutationBackendSelection::Verified(verified) =
            BackendSelection::verified_for_mutation(&base, None::<&ParserBackendConfig>).unwrap()
        else {
            panic!("verified")
        };
        accepts_configured(&verified.configured);
        let explicit = BackendSelection::explicit_url(&StateUrl::parse("state.json").unwrap());
        assert!(matches!(explicit.url, StateUrl::File { .. }));
    }
    #[tokio::test]
    async fn review_r1_resolved_read_origin_is_implicit_declared_or_locked() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("main.crn"), "let unused = 'fixture'\n").unwrap();
        let base = ResolvedPath::new(root.path()).unwrap();
        let read = BackendSelection::for_read(&base, None::<&ParserBackendConfig>)
            .unwrap()
            .resolve_with_drift_warning(|_| {})
            .await
            .unwrap();
        assert_eq!(read.origin(), ReadBackendOrigin::Implicit);
        let declared = raw("new.json");
        let read = BackendSelection::for_read(&base, Some(&declared))
            .unwrap()
            .resolve_with_drift_warning(|_| {})
            .await
            .unwrap();
        assert_eq!(read.origin(), ReadBackendOrigin::Declared);
        BackendLock::for_config(Some(&raw("old.json")))
            .unwrap()
            .save(&base)
            .unwrap();
        let read = BackendSelection::for_read(&base, Some(&declared))
            .unwrap()
            .resolve_with_drift_warning(|_| {})
            .await
            .unwrap();
        assert_eq!(read.origin(), ReadBackendOrigin::Locked);
        assert_eq!(read.selected_config().as_s3(), None);
        let BackendConfig::Local(settings) = read.selected_config() else {
            panic!("local")
        };
        assert_eq!(settings.path(), Some("old.json"));
    }
    #[tokio::test]
    async fn review_r1_migration_locate_error_matches_main_text() {
        let root = tempfile::tempdir().unwrap();
        let base = ResolvedPath::new(root.path()).unwrap();
        let settings = BackendConfig::try_from(&raw("bad\0state")).unwrap();
        let BackendConfig::Local(local) = &settings else {
            panic!("local")
        };
        let io = crate::backends::anchored_local_path(local, &base).unwrap_err();
        let selected = ConfiguredBackend::new(AdmittedBackendOrigin::Declared(settings), base);
        let error = resolve_migration_backend(selected.config(), &selected.base_dir)
            .await
            .err()
            .unwrap();
        assert_eq!(error.to_string(), format!("I/O error: {io}"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn review_r1_implicit_locate_error_matches_main_text() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let base = ResolvedPath::new(root.path()).unwrap();
        let name = crate::backends::LocalBackend::DEFAULT_STATE_FILE;
        symlink(name, root.path().join(name)).unwrap();
        let io = base.join(name).unwrap_err();
        let read = BackendSelection::for_read(&base, None::<&ParserBackendConfig>).unwrap();
        let error = read.resolve_with_drift_warning(|_| {}).await.err().unwrap();
        assert_eq!(error.to_string(), format!("I/O error: {io}"));
    }
}

#[cfg(test)]
mod round3_migration_origin_tests {
    use super::*;
    #[tokio::test]
    async fn round3_migration_consumes_settings_without_labeling_an_origin() {
        let root = tempfile::tempdir().unwrap();
        let base = ResolvedPath::new(root.path()).unwrap();
        let config = BackendConfig::Local(BackendConfig::implicit_local());
        let (_, path) = resolve_migration_backend(&config, &base).await.unwrap();
        assert_eq!(
            path.unwrap().identity(),
            base.join("carina.state.json").unwrap().identity()
        );
    }
}
