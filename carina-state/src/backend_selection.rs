//! Typed selection of the state backend an operation is allowed to open.
//!
//! Backend construction accepts only the opaque [`BackendSelection`] token.
//! Callers obtain that token through an operation-specific selector, making
//! lock-aware reads, verified mutations, explicit URLs, and deliberately
//! unlocked configuration access distinct at the call site.

use std::path::{Path, PathBuf};

use carina_core::parser::BackendConfig as ParserBackendConfig;

use crate::backend::{BackendError, BackendResult};
use crate::backend_config::{BackendConfig, S3BackendSettings};
use crate::backend_lock::BackendLock;
use crate::backends::StateUrl;

/// The old and new backend addresses involved in a pending migration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendDrift {
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
    pub fn backend_selection(&self) -> &BackendSelection {
        &self.selection
    }

    pub fn selected_config(&self) -> Option<&BackendConfig> {
        self.selection.configured_config()
    }

    pub fn lock_present(&self) -> bool {
        self.lock_present
    }

    pub fn drift(&self) -> Option<&BackendDrift> {
        self.drift.as_ref()
    }
}

/// Selection produced only after the backend lock exists and matches the
/// configured backend.
#[derive(Debug, Clone)]
pub struct VerifiedMutationBackendSelection {
    selection: BackendSelection,
}

impl VerifiedMutationBackendSelection {
    pub fn backend_selection(&self) -> &BackendSelection {
        &self.selection
    }

    pub fn s3_settings(&self) -> Option<&S3BackendSettings> {
        self.selection
            .configured_config()
            .and_then(BackendConfig::as_s3)
    }

    pub async fn resolve(&self) -> BackendResult<Box<dyn crate::StateBackend>> {
        crate::backends::create_backend(self.backend_selection()).await
    }
}

/// Exhaustive result of attempting to select a mutation backend.
#[derive(Debug)]
pub enum MutationBackendSelection {
    Verified(VerifiedMutationBackendSelection),
    MissingLock { path: PathBuf },
    Drifted(BackendDrift),
}

/// Explicit opt-out from backend-lock selection for operations whose contract
/// intentionally targets the supplied configuration, such as migration
/// endpoints and state-bucket deletion.
#[derive(Debug, Clone)]
pub struct UnlockedConfiguredBackendSelection {
    selection: BackendSelection,
}

impl UnlockedConfiguredBackendSelection {
    pub fn backend_selection(&self) -> &BackendSelection {
        &self.selection
    }
}

/// Selection for a user-supplied `--state-url`.
#[derive(Debug, Clone)]
pub struct ExplicitUrlBackendSelection {
    selection: BackendSelection,
}

impl ExplicitUrlBackendSelection {
    pub fn backend_selection(&self) -> &BackendSelection {
        &self.selection
    }
}

/// Lock-aware backend selection for the administrative force-unlock path.
/// It uses the same old-backend-on-drift rule as reads but deliberately leaves
/// warning policy to the caller.
#[derive(Debug, Clone)]
pub struct ForceUnlockBackendSelection {
    selection: BackendSelection,
}

impl ForceUnlockBackendSelection {
    pub fn backend_selection(&self) -> &BackendSelection {
        &self.selection
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
    pub fn for_read(
        project_dir: &Path,
        configured: Option<&ParserBackendConfig>,
    ) -> BackendResult<ReadBackendSelection> {
        let project_dir = canonical_project_dir(project_dir);
        let inspection = inspect(&project_dir, configured)?;
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
    pub fn verified_for_mutation(
        project_dir: &Path,
        configured: Option<&ParserBackendConfig>,
    ) -> BackendResult<MutationBackendSelection> {
        let project_dir = canonical_project_dir(project_dir);
        let inspection = inspect(&project_dir, configured)?;
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

    /// Select the configured address without consulting the backend lock.
    ///
    /// This is intentionally conspicuous: use it only when the operation is
    /// defined to target a migration endpoint or the newly configured backend.
    pub fn unlocked_configured(
        project_dir: &Path,
        configured: Option<&BackendConfig>,
    ) -> UnlockedConfiguredBackendSelection {
        UnlockedConfiguredBackendSelection {
            selection: Self::configured_target(
                configured.cloned(),
                canonical_project_dir(project_dir),
            ),
        }
    }

    /// Select the exact URL supplied by the user, bypassing project backend
    /// configuration and its lock by explicit contract.
    pub fn explicit_url(url: &StateUrl) -> ExplicitUrlBackendSelection {
        ExplicitUrlBackendSelection {
            selection: Self {
                target: BackendTarget::ExplicitUrl(url.clone()),
            },
        }
    }

    /// Select the backend whose lock may be administratively force-released.
    /// On drift this is the old locked backend, matching read selection.
    pub fn for_force_unlock(
        project_dir: &Path,
        configured: Option<&ParserBackendConfig>,
    ) -> BackendResult<ForceUnlockBackendSelection> {
        let selected = Self::for_read(project_dir, configured)?;
        Ok(ForceUnlockBackendSelection {
            selection: selected.selection,
        })
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
pub fn inspect_backend_drift(
    project_dir: &Path,
    configured: Option<&ParserBackendConfig>,
) -> BackendResult<BackendDriftStatus> {
    inspect(project_dir, configured).map(|inspection| inspection.status)
}

fn inspect(
    project_dir: &Path,
    configured: Option<&ParserBackendConfig>,
) -> BackendResult<BackendInspection> {
    let project_dir = project_dir.to_path_buf();
    let configured = configured
        .map(BackendConfig::try_from)
        .transpose()
        .map_err(BackendError::from)?;
    let configured_lock = configured
        .as_ref()
        .map(BackendLock::from_config)
        .unwrap_or_else(BackendLock::local_default);
    let existing = BackendLock::load(&project_dir)?;
    let status = match existing {
        None => BackendDriftStatus::Fresh,
        Some(existing) if existing == configured_lock => BackendDriftStatus::Unchanged,
        Some(existing) => BackendDriftStatus::Drifted(BackendDrift {
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

    #[test]
    fn read_selection_uses_locked_config_on_drift() {
        let project = tempfile::tempdir().unwrap();
        BackendLock::for_config(Some(&local_config("old.json")))
            .unwrap()
            .save(project.path())
            .unwrap();

        let selected =
            BackendSelection::for_read(project.path(), Some(&local_config("new.json"))).unwrap();

        let Some(BackendConfig::Local(settings)) = selected.selected_config() else {
            panic!("read must select the old local backend")
        };
        assert_eq!(settings.path(), Some("old.json"));
        assert!(selected.drift().is_some());
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

    #[cfg(unix)]
    #[test]
    fn read_selection_canonicalizes_the_backend_base_directory() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let alias = root.path().join("project-alias");
        symlink(&project, &alias).unwrap();

        let selected = BackendSelection::for_read(&alias, None).unwrap();
        let BackendTarget::Configured { base_dir, .. } = selected.backend_selection().target()
        else {
            panic!("project read must select a configured backend target")
        };
        assert_eq!(base_dir, &project.canonicalize().unwrap());
    }
}
