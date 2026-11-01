//! Typed selection of the state backend an operation is allowed to open.
//!
//! Raw backend construction is crate-private. Public callers obtain only an
//! operation-specific capability through [`BackendSelection`], keeping
//! lock-aware reads, verified mutations, migrations, force-unlock, and
//! state-bucket deletion distinct at the call site.

use std::path::{Path, PathBuf};

use carina_core::parser::BackendConfig as ParserBackendConfig;
use carina_core::resource::{BindingAddress, PlaceholderKind};
use thiserror::Error;

use crate::backend::{BackendError, BackendResult};
use crate::backend_config::{BackendConfig, S3BackendSettings};
use crate::backend_lock::BackendLock;
use crate::backends::{S3_BACKEND_PROVIDER_NAME, S3_BACKEND_RESOURCE_TYPE, StateUrl};

/// The old and new backend addresses involved in a pending migration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendDrift {
    project_dir: PathBuf,
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
    Drifted(BackendDrift),
}

/// Opaque proof that a backend address was chosen through a named selection
/// path rather than constructed directly from raw configuration.
#[derive(Debug, Clone)]
pub struct BackendSelection {
    target: BackendTarget,
}

#[derive(Debug, Clone)]
pub(crate) enum BackendTarget {
    Configured {
        config: Option<BackendConfig>,
        base_dir: PathBuf,
    },
    ExplicitUrl(StateUrl),
}

/// Lock-aware selection for a read-only operation.
#[derive(Debug, Clone)]
pub struct ReadBackendSelection {
    selection: BackendSelection,
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
        let selected_config = self.selection.configured_config().cloned();
        let migration_pending = self.drift.is_some();
        let backend = crate::backends::create_backend(&self.selection).await?;
        if let Some(drift) = self.drift.as_ref() {
            warn(drift);
        }
        Ok(ResolvedReadBackend {
            backend,
            selected_config,
            migration_pending,
        })
    }
}

/// A backend narrowed to the operations used by read-only commands.
pub struct ResolvedReadBackend {
    backend: Box<dyn crate::StateBackend>,
    selected_config: Option<BackendConfig>,
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

    pub fn selected_config(&self) -> Option<&BackendConfig> {
        self.selected_config.as_ref()
    }

    pub fn migration_pending(&self) -> bool {
        self.migration_pending
    }
}

/// Selection produced only after the backend lock exists and matches the
/// configured backend.
#[derive(Debug, Clone)]
pub struct VerifiedMutationBackendSelection {
    selection: BackendSelection,
}

impl VerifiedMutationBackendSelection {
    pub fn s3_settings(&self) -> Option<&S3BackendSettings> {
        self.selection
            .configured_config()
            .and_then(BackendConfig::as_s3)
    }

    pub async fn resolve(&self) -> BackendResult<Box<dyn crate::StateBackend>> {
        crate::backends::create_backend(&self.selection).await
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
/// inspect/write/verify the target, and inspect the source configuration for
/// post-commit local cleanup.
pub struct MigrationBackends {
    source: Box<dyn crate::StateBackend>,
    target: Box<dyn crate::StateBackend>,
    source_config: Option<BackendConfig>,
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

    pub fn source_config(&self) -> Option<&BackendConfig> {
        self.source_config.as_ref()
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
    selection: BackendSelection,
}

impl ExplicitUrlBackendSelection {
    pub(crate) async fn read_state(&self) -> BackendResult<Option<crate::LoadedState>> {
        crate::backends::create_backend(&self.selection)
            .await?
            .read_state()
            .await
    }

    pub(crate) async fn read_published_exports(
        &self,
    ) -> BackendResult<Option<crate::PublishedExports>> {
        let backend = crate::backends::create_backend(&self.selection).await?;
        crate::backend::read_published_exports(backend.as_ref()).await
    }
}

struct BackendInspection {
    project_dir: PathBuf,
    configured: Option<BackendConfig>,
    status: BackendDriftStatus,
}

impl BackendSelection {
    /// Select a backend for a project-backed read.
    ///
    /// A pending migration selects the old backend recorded in the lock and
    /// returns its drift metadata without printing. Missing locks are allowed
    /// here so the command layer can retain command-specific policy.
    pub fn for_read<P: PlaceholderKind, B: BindingAddress>(
        project_dir: &Path,
        configured: Option<&ParserBackendConfig<P, B>>,
    ) -> BackendResult<ReadBackendSelection> {
        let inspection = inspect(&canonical_project_dir(project_dir), configured)?;
        Self::for_read_inspection(inspection)
    }

    fn for_read_inspection(inspection: BackendInspection) -> BackendResult<ReadBackendSelection> {
        let lock_present = !matches!(inspection.status, BackendDriftStatus::Fresh);
        let (selected_config, drift) = match inspection.status {
            BackendDriftStatus::Drifted(drift) => {
                let config = drift.existing.to_state_config()?;
                (Some(config), Some(drift))
            }
            BackendDriftStatus::Fresh | BackendDriftStatus::Unchanged => {
                (inspection.configured, None)
            }
        };
        Ok(ReadBackendSelection {
            selection: Self::configured_target(selected_config, inspection.project_dir),
            lock_present,
            drift,
        })
    }

    /// Select a backend for an operation that may mutate state.
    ///
    /// Only the `Verified` result carries a constructible backend token.
    pub fn verified_for_mutation<P: PlaceholderKind, B: BindingAddress>(
        project_dir: &Path,
        configured: Option<&ParserBackendConfig<P, B>>,
    ) -> BackendResult<MutationBackendSelection> {
        let inspection = inspect(&canonical_project_dir(project_dir), configured)?;
        Self::verified_for_mutation_inspection(inspection)
    }

    fn verified_for_mutation_inspection(
        inspection: BackendInspection,
    ) -> BackendResult<MutationBackendSelection> {
        match inspection.status {
            BackendDriftStatus::Fresh => Ok(MutationBackendSelection::MissingLock {
                path: BackendLock::lock_path(&inspection.project_dir),
            }),
            BackendDriftStatus::Unchanged => Ok(MutationBackendSelection::Verified(
                VerifiedMutationBackendSelection {
                    selection: Self::configured_target(
                        inspection.configured,
                        inspection.project_dir,
                    ),
                },
            )),
            BackendDriftStatus::Drifted(drift) => Ok(MutationBackendSelection::Drifted(drift)),
        }
    }

    /// Resolve the old and new backend addresses carried by a proven drift.
    pub async fn for_migration(drift: &BackendDrift) -> BackendResult<MigrationBackends> {
        let project_dir = canonical_project_dir(&drift.project_dir);
        let source =
            Self::configured_target(Some(drift.existing.to_state_config()?), project_dir.clone());
        let target =
            Self::configured_target(Some(drift.configured.to_state_config()?), project_dir);
        let source_config = source.configured_config().cloned();
        Ok(MigrationBackends {
            source: crate::backends::create_backend(&source).await?,
            target: crate::backends::create_backend(&target).await?,
            source_config,
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
        ExplicitUrlBackendSelection {
            selection: Self {
                target: BackendTarget::ExplicitUrl(url.clone()),
            },
        }
    }

    /// Force-release a backend lock, selecting the old locked backend during
    /// pending migration without exposing a general backend capability.
    pub async fn force_unlock<P: PlaceholderKind, B: BindingAddress>(
        project_dir: &Path,
        configured: Option<&ParserBackendConfig<P, B>>,
        lock_id: &str,
    ) -> BackendResult<()> {
        let selected = Self::for_read(project_dir, configured)?;
        crate::backends::create_backend(&selected.selection)
            .await?
            .force_unlock(lock_id)
            .await
    }

    pub(crate) fn target(&self) -> &BackendTarget {
        &self.target
    }

    fn configured_target(config: Option<BackendConfig>, base_dir: PathBuf) -> Self {
        Self {
            target: BackendTarget::Configured { config, base_dir },
        }
    }

    fn configured_config(&self) -> Option<&BackendConfig> {
        match &self.target {
            BackendTarget::Configured { config, .. } => config.as_ref(),
            BackendTarget::ExplicitUrl(_) => None,
        }
    }
}

/// Inspect backend drift without constructing a backend.
pub fn inspect_backend_drift<P: PlaceholderKind, B: BindingAddress>(
    project_dir: &Path,
    configured: Option<&ParserBackendConfig<P, B>>,
) -> BackendResult<BackendDriftStatus> {
    inspect(project_dir, configured).map(|inspection| inspection.status)
}

fn inspect<P: PlaceholderKind, B: BindingAddress>(
    project_dir: &Path,
    configured: Option<&ParserBackendConfig<P, B>>,
) -> BackendResult<BackendInspection> {
    let configured = configured
        .map(BackendConfig::try_from)
        .transpose()
        .map_err(BackendError::from)?;
    inspect_resolved(project_dir, configured)
}

fn inspect_resolved(
    project_dir: &Path,
    configured: Option<BackendConfig>,
) -> BackendResult<BackendInspection> {
    let project_dir = project_dir.to_path_buf();
    let configured_lock = configured
        .as_ref()
        .map(BackendLock::from_config)
        .unwrap_or_else(BackendLock::local_default);
    let existing = BackendLock::load(&project_dir)?;
    let status = match existing {
        None => BackendDriftStatus::Fresh,
        Some(existing) if existing == configured_lock => BackendDriftStatus::Unchanged,
        Some(existing) => BackendDriftStatus::Drifted(BackendDrift {
            project_dir: project_dir.clone(),
            existing,
            configured: configured_lock,
        }),
    };
    Ok(BackendInspection {
        project_dir,
        configured,
        status,
    })
}

fn canonical_project_dir(project_dir: &Path) -> PathBuf {
    project_dir
        .canonicalize()
        .unwrap_or_else(|_| project_dir.to_path_buf())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use carina_core::resource::{ConcreteValue, Value};

    use super::*;

    fn local_config(path: &str) -> ParserBackendConfig {
        ParserBackendConfig {
            backend_type: "local".to_string(),
            attributes: HashMap::from([(
                "path".to_string(),
                Value::Concrete(ConcreteValue::String(path.to_string().into())),
            )]),
            attribute_spans: HashMap::new(),
        }
    }

    fn s3_config(bucket: &str) -> ParserBackendConfig {
        ParserBackendConfig {
            backend_type: "s3".to_string(),
            attributes: HashMap::from([
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
            ]),
            attribute_spans: HashMap::new(),
        }
    }

    #[tokio::test]
    async fn resolved_read_carries_locked_config_and_drift() {
        let project = tempfile::tempdir().unwrap();
        BackendLock::for_config(Some(&local_config("old.json")))
            .unwrap()
            .save(project.path())
            .unwrap();

        let selected =
            BackendSelection::for_read(project.path(), Some(&local_config("new.json"))).unwrap();
        let resolved = selected.resolve_with_drift_warning(|_| {}).await.unwrap();

        let Some(BackendConfig::Local(settings)) = resolved.selected_config() else {
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
            .save(project.path())
            .unwrap();
        let selected =
            BackendSelection::for_read(project.path(), Some(&local_config("new.json"))).unwrap();
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
            .save(project.path())
            .unwrap();

        let selected = BackendSelection::verified_for_mutation(
            project.path(),
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
            .save(project.path())
            .unwrap();
        let BackendDriftStatus::Drifted(drift) =
            inspect_backend_drift(project.path(), Some(&local_config("new.json"))).unwrap()
        else {
            panic!("fixture must have pending backend migration")
        };

        let backends = BackendSelection::for_migration(&drift).await.unwrap();

        let Some(BackendConfig::Local(source)) = backends.source_config() else {
            panic!("migration source must be the locked local backend")
        };
        assert_eq!(source.path(), Some("old.json"));
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
    fn read_selection_canonicalizes_the_backend_base_directory() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let alias = root.path().join("project-alias");
        symlink(&project, &alias).unwrap();

        let selected = BackendSelection::for_read(&alias, None::<&ParserBackendConfig>).unwrap();
        let BackendTarget::Configured { base_dir, .. } = selected.selection.target() else {
            panic!("project read must select a configured backend target")
        };
        assert_eq!(base_dir, &project.canonicalize().unwrap());
    }
}
