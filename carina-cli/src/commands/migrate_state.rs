//! Backend state migration for `carina init --migrate-state`.
//!
//! When the configured backend in `backend.crn` differs from the address
//! recorded in `carina-backend.lock`, the state file must be moved from
//! the *locked* (old) backend to the *configured* (new) backend before
//! plan/apply will operate on the right state. This module performs that
//! move.
//!
//! Scope (issue #3160, option 1 — "comment 2 surface only"):
//!
//! - Single-process ordering: read source → guard target → write target →
//!   verify roundtrip → rewrite `carina-backend.lock` → optionally delete
//!   the source.
//! - Target guard: refuse to overwrite a non-empty, differently-lineaged
//!   target unless `--force`.
//! - The full dual-backend distributed lock + half-migration recovery
//!   path from the issue's first comment is intentionally out of scope
//!   and tracked as a separate follow-up issue.

use std::path::Path;

use crate::output_style::Colorize;

use carina_state::{
    BackendConfig as StateBackendConfig, BackendDriftStatus, BackendSelection, MigrationBackends,
    StateFile, anchored_local_path, inspect_backend_drift,
};
#[cfg(test)]
use carina_state::{BackendLock, LocalBackend};

#[cfg(test)]
use super::local_backend_for_test;
use crate::error::AppError;
use carina_core::hint::ProjectCommand;

/// What happened to the old (source) state after a committed migration.
///
/// Distinguishing these is what lets the caller print the *right*
/// follow-up: a kept remote backup vs. a failed local cleanup are very
/// different user situations and must not collapse to one bool.
#[derive(Debug, PartialEq, Eq)]
pub enum SourceDisposition {
    /// Local source removed after the verified copy (nothing to do).
    Deleted,
    /// Remote source intentionally kept as a recoverable backup; the
    /// user should remove it once they trust the new backend.
    KeptAsBackup,
    /// Local source removal was attempted and failed; the migration is
    /// already committed but a stale old file remains. `run_init_migrate_state`
    /// has already warned about this on stderr.
    DeleteFailed,
}

/// Outcome of a migration attempt, returned so callers (and tests) can
/// assert on what happened without scraping stdout.
#[derive(Debug, PartialEq, Eq)]
pub enum MigrationOutcome {
    /// Locked and configured backends are identical — nothing to do.
    NotNeeded,
    /// State was copied and the lock rewritten. `source` records what
    /// happened to the old state afterwards.
    Migrated {
        resources: usize,
        source: SourceDisposition,
    },
}

/// Migrate state through purpose-limited source and target capabilities.
///
/// This is the testable core: it takes the migration backend pair and the
/// overwrite policy and performs the read → guard → write → verify sequence.
/// Lock-file rewriting and source cleanup are the caller's responsibility.
///
/// `force` allows overwriting a target that already contains a
/// *different* state (different lineage or non-empty resources). Without
/// it, a populated target aborts the migration loudly.
async fn perform_state_migration(
    backends: &MigrationBackends,
    force: bool,
    project_dir: &Path,
) -> Result<StateFile, AppError> {
    // `carina init --migrate-state` copies the source state into the
    // target backend verbatim. The pending schema-migration marker is
    // discarded here: writing the target state will re-emit the
    // state at `StateFile::CURRENT_VERSION` on disk regardless, and
    // the `--migrate-state` command is itself the backend-relocation
    // path (carina#3315 — schema-migration persistence is handled by
    // apply/destroy/state-refresh in the source-or-target directory).
    let state = backends
        .read_source_state()
        .await
        .map_err(AppError::Backend)?
        .map(|loaded| loaded.into_state())
        .ok_or_else(|| {
            AppError::Config(format!(
                "The locked backend holds no state. There is nothing to migrate; \
                 re-run `{}` (without --migrate-state) to adopt the new \
                 backend, or revert the backend configuration.",
                ProjectCommand::new("init", project_dir)
            ))
        })?;

    // Target guard: refuse to clobber a populated, differently-lineaged
    // target unless the operator explicitly opted in with --force.
    if let Some(existing) = backends
        .read_target_state()
        .await
        .map_err(AppError::Backend)?
        .map(|loaded| loaded.into_state())
        && (existing.lineage != state.lineage || !existing.resources().is_empty())
        && !force
    {
        return Err(AppError::Config(format!(
            "The configured backend already contains state (lineage {}, {} \
             resources). Refusing to overwrite it. If this target is a stale \
             copy you want to replace, re-run with --force; otherwise verify \
             the backend configuration points where you expect.",
            existing.lineage,
            existing.resources().len()
        )));
    }

    backends
        .write_target_state(&state)
        .await
        .map_err(AppError::Backend)?;

    // Verify the copy landed before we rewrite the lock / delete the
    // source — a migration that silently lost resources is the worst
    // possible outcome.
    let roundtrip = backends
        .read_target_state()
        .await
        .map_err(AppError::Backend)?
        .map(|loaded| loaded.into_state())
        .ok_or_else(|| {
            AppError::Config(
                "Failed to read state back from the configured backend after \
                 writing. The migration was aborted before touching the lock \
                 or the source."
                    .to_string(),
            )
        })?;
    if roundtrip.lineage != state.lineage || roundtrip.resources().len() != state.resources().len()
    {
        return Err(AppError::Config(
            "State verification failed: the copy read back from the configured \
             backend did not match the source. The source is untouched."
                .to_string(),
        ));
    }

    Ok(state)
}

/// Entry point used by `carina init --migrate-state`.
///
/// Compares `carina-backend.lock` against the configured backend; if they
/// differ, migrates state from the locked address to the configured one,
/// then rewrites the lock. A no-op (returns [`MigrationOutcome::NotNeeded`])
/// when they already match.
pub async fn run_init_migrate_state(
    color_mode: crate::output_style::ColorMode,
    project_dir: &Path,
    backend_config: Option<&carina_core::parser::BackendConfig>,
    force: bool,
) -> Result<MigrationOutcome, AppError> {
    let drift = match inspect_backend_drift(project_dir, backend_config)? {
        BackendDriftStatus::Fresh => {
            return Err(AppError::Config(format!(
                "No backend lock found. Run `{}` (without \
                 --migrate-state) to initialize the project first.",
                ProjectCommand::new("init", project_dir)
            )));
        }
        BackendDriftStatus::Unchanged => return Ok(MigrationOutcome::NotNeeded),
        BackendDriftStatus::Drifted(drift) => drift,
    };

    println!(
        "{}",
        "Backend configuration changed; migrating state:"
            .cyan(color_mode)
            .bold(color_mode)
    );
    println!("{}", drift.existing().describe_diff(drift.configured()));

    // The typed drift above resolves directly to purpose-limited migration
    // backends: the locked OLD source and configured NEW target, both anchored
    // at the project directory.
    let backends = BackendSelection::for_migration(&drift)
        .await
        .map_err(AppError::Backend)?;

    let state = perform_state_migration(&backends, force, project_dir).await?;
    println!(
        "  {} copied {} resource(s) to the configured backend",
        "✓".green(color_mode),
        state.resources().len()
    );

    // Rewrite the lock *before* touching the source. This is the commit
    // point: until it lands, both the lock and the (untouched) source
    // still describe the old backend, and the source is never destroyed,
    // so the state is never lost. After it lands the migration is
    // logically complete and idempotent — a re-run sees `locked ==
    // configured` and is a `NotNeeded` no-op. (A failure of this save
    // itself leaves the verified copy at the target and the source
    // intact; re-running re-copies onto the now-identical target, which
    // needs `--force` because the target is non-empty — recovery of that
    // narrow window is the deferred follow-up, not silent data loss.
    // Deleting the source first, as an earlier revision did, could
    // instead strand the project: a crash between the delete and the
    // lock rewrite would point the lock at a now-missing source.)
    drift
        .configured()
        .save(project_dir)
        .map_err(AppError::Backend)?;
    println!("  {} updated backend lock", "✓".green(color_mode));

    // A local source is deleted after the commit (matches the retired
    // `state migrate`); a remote source is kept as a recoverable backup
    // (remote → remote in this scope). A failure to delete here is *not*
    // fatal: the migration already committed, so a leftover old file is
    // harmless and the next run is a no-op.
    let source = if let Some(StateBackendConfig::Local(local_settings)) = backends.source_config() {
        let path = anchored_local_path(local_settings, project_dir);
        if !path.exists() {
            SourceDisposition::Deleted
        } else {
            match std::fs::remove_file(&path) {
                Ok(()) => {
                    println!(
                        "  {} deleted source local state file",
                        "✓".green(color_mode)
                    );
                    SourceDisposition::Deleted
                }
                Err(e) => {
                    eprintln!(
                        "  {} migration committed, but the old state file {} \
                         could not be removed ({e}); delete it manually.",
                        "warning:".yellow(color_mode),
                        path.display()
                    );
                    SourceDisposition::DeleteFailed
                }
            }
        }
    } else {
        SourceDisposition::KeptAsBackup
    };

    Ok(MigrationOutcome::Migrated {
        resources: state.resources().len(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use carina_core::resource::{ConcreteValue, Value};
    use carina_state::ResourceState;
    use std::collections::HashMap;

    fn state_with(lineage: &str, n_resources: usize) -> StateFile {
        let mut s = StateFile::new();
        s.lineage = lineage.to_string();
        for i in 0..n_resources {
            // A current managed instance carries the identifier returned by
            // its provider mutation. `check_and_migrate` preserves malformed
            // identifier-less rows so the typed plan boundary can fail closed;
            // historical data-source artifacts are removed only after parsing
            // supplies their exact typed `read` addresses (carina#3266).
            s.upsert_resource(
                ResourceState::try_applied(
                    "s3.Bucket",
                    carina_core::resource::ResourceIdentity::try_from(format!("r{i}"))
                        .expect("formatted test identity is non-empty"),
                    "aws",
                    format!("bucket-{i}"),
                )
                .expect("fixture identifier is non-empty"),
            )
            .expect("test state setup must be valid");
        }
        s
    }

    fn local_config(path: &Path) -> carina_core::parser::BackendConfig {
        carina_core::parser::BackendConfig {
            backend_type: "local".to_string(),
            attributes: HashMap::from([(
                "path".to_string(),
                Value::Concrete(ConcreteValue::String(
                    path.to_string_lossy().into_owned().into(),
                )),
            )]),
            attribute_spans: HashMap::new(),
        }
    }

    async fn migration_backends(
        project_dir: &Path,
        source_path: &Path,
        target_path: &Path,
    ) -> MigrationBackends {
        BackendLock::for_config(Some(&local_config(source_path)))
            .unwrap()
            .save(project_dir)
            .unwrap();
        let BackendDriftStatus::Drifted(drift) =
            inspect_backend_drift(project_dir, Some(&local_config(target_path))).unwrap()
        else {
            panic!("test fixture must describe a pending backend migration")
        };
        BackendSelection::for_migration(&drift).await.unwrap()
    }

    #[tokio::test]
    async fn migrates_state_from_source_to_empty_target() {
        let tmp = tempfile::tempdir().unwrap();
        let src_path = tmp.path().join("old.state.json");
        let dst_path = tmp.path().join("new.state.json");

        let src = local_backend_for_test(src_path.clone());
        let dst = local_backend_for_test(dst_path.clone());
        src.write_state(&state_with("lin-1", 3)).await.unwrap();
        let backends = migration_backends(tmp.path(), &src_path, &dst_path).await;

        let state = perform_state_migration(&backends, false, tmp.path())
            .await
            .unwrap();
        assert_eq!(state.resources().len(), 3);

        let migrated = dst.read_state().await.unwrap().unwrap().into_state();
        assert_eq!(migrated.lineage, "lin-1");
        assert_eq!(migrated.resources().len(), 3);
    }

    #[tokio::test]
    async fn refuses_populated_target_without_force() {
        let tmp = tempfile::tempdir().unwrap();
        let src_path = tmp.path().join("a.json");
        let dst_path = tmp.path().join("b.json");
        let src = local_backend_for_test(src_path.clone());
        let dst = local_backend_for_test(dst_path.clone());
        src.write_state(&state_with("lin-src", 1)).await.unwrap();
        dst.write_state(&state_with("lin-dst", 2)).await.unwrap();
        let backends = migration_backends(tmp.path(), &src_path, &dst_path).await;

        let err = perform_state_migration(&backends, false, tmp.path())
            .await
            .unwrap_err();
        assert!(
            matches!(&err, AppError::Config(m) if m.contains("already contains state")),
            "got: {err:?}"
        );
        // Target must be untouched on refusal.
        let dst_state = dst.read_state().await.unwrap().unwrap().into_state();
        assert_eq!(dst_state.lineage, "lin-dst");
    }

    #[tokio::test]
    async fn force_overwrites_populated_target() {
        let tmp = tempfile::tempdir().unwrap();
        let src_path = tmp.path().join("a.json");
        let dst_path = tmp.path().join("b.json");
        let src = local_backend_for_test(src_path.clone());
        let dst = local_backend_for_test(dst_path.clone());
        src.write_state(&state_with("lin-src", 1)).await.unwrap();
        dst.write_state(&state_with("lin-dst", 2)).await.unwrap();
        let backends = migration_backends(tmp.path(), &src_path, &dst_path).await;

        perform_state_migration(&backends, true, tmp.path())
            .await
            .unwrap();
        let dst_state = dst.read_state().await.unwrap().unwrap().into_state();
        assert_eq!(dst_state.lineage, "lin-src");
        assert_eq!(dst_state.resources().len(), 1);
    }

    #[tokio::test]
    async fn force_overwrites_higher_serial_same_lineage_target_from_fresh_backend() {
        let tmp = tempfile::tempdir().unwrap();
        let src_path = tmp.path().join("a.json");
        let dst_path = tmp.path().join("b.json");

        let mut source_state = state_with("lin-shared", 1);
        source_state.increment_serial();
        local_backend_for_test(src_path.clone())
            .write_state(&source_state)
            .await
            .unwrap();

        let mut target_state = state_with("lin-shared", 2);
        target_state.increment_serial();
        target_state.increment_serial();
        local_backend_for_test(dst_path.clone())
            .write_state(&target_state)
            .await
            .unwrap();

        let backends = migration_backends(tmp.path(), &src_path, &dst_path).await;
        perform_state_migration(&backends, true, tmp.path())
            .await
            .unwrap();

        let migrated = local_backend_for_test(dst_path)
            .read_state()
            .await
            .unwrap()
            .unwrap()
            .into_state();
        assert_eq!(migrated.lineage, source_state.lineage);
        assert_eq!(migrated.serial, source_state.serial);
        assert_eq!(migrated.resources().len(), 1);
    }

    #[tokio::test]
    async fn errors_when_source_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let src_path = tmp.path().join("a.json");
        let dst_path = tmp.path().join("b.json");
        let backends = migration_backends(tmp.path(), &src_path, &dst_path).await;

        let err = perform_state_migration(&backends, false, tmp.path())
            .await
            .unwrap_err();
        assert!(
            matches!(&err, AppError::Config(m) if m.contains("no state")),
            "got: {err:?}"
        );
    }

    #[tokio::test]
    async fn not_needed_when_lock_matches_configured_local() {
        let color_mode = crate::output_style::ColorMode::Plain;
        let tmp = tempfile::tempdir().unwrap();
        // Lock = local_default; configured = None (also local default).
        BackendLock::local_default().save(tmp.path()).unwrap();
        let outcome = run_init_migrate_state(color_mode, tmp.path(), None, false)
            .await
            .unwrap();
        assert_eq!(outcome, MigrationOutcome::NotNeeded);
    }

    #[tokio::test]
    async fn local_to_local_migration_deletes_source() {
        let color_mode = crate::output_style::ColorMode::Plain;
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path();
        // Locked = local default (carina.state.json under base_dir).
        BackendLock::local_default().save(base).unwrap();
        let src_path = base.join(LocalBackend::DEFAULT_STATE_FILE);
        local_backend_for_test(src_path.clone())
            .write_state(&state_with("lin-x", 2))
            .await
            .unwrap();

        // Configured = a *different* local path (still "local" type but
        // different attributes ⇒ lock differs ⇒ migration runs).
        let dst_path = base.join("relocated.state.json");
        let cfg = carina_core::parser::BackendConfig {
            backend_type: "local".to_string(),
            attributes: {
                let mut m = HashMap::new();
                m.insert(
                    "path".to_string(),
                    Value::Concrete(ConcreteValue::String(
                        dst_path.to_string_lossy().into_owned().into(),
                    )),
                );
                m
            },
            attribute_spans: HashMap::new(),
        };

        let outcome = run_init_migrate_state(color_mode, base, Some(&cfg), false)
            .await
            .unwrap();
        assert_eq!(
            outcome,
            MigrationOutcome::Migrated {
                resources: 2,
                source: SourceDisposition::Deleted
            }
        );
        assert!(
            !src_path.exists(),
            "source should be deleted (local cleanup)"
        );
        let migrated = local_backend_for_test(dst_path)
            .read_state()
            .await
            .unwrap()
            .unwrap()
            .into_state();
        assert_eq!(migrated.resources().len(), 2);

        // Lock now reflects the configured backend ⇒ a second run is a no-op.
        let second = run_init_migrate_state(color_mode, base, Some(&cfg), false)
            .await
            .unwrap();
        assert_eq!(second, MigrationOutcome::NotNeeded);
    }

    /// Regression: the lock must be committed to the *configured*
    /// address before the source is removed. Concretely, once a
    /// migration returns, the on-disk lock equals the configured lock,
    /// so even with the source gone a re-run is a clean `NotNeeded`
    /// rather than the old "locked points at a now-missing source"
    /// wedge. (Round-3 finding: an earlier revision deleted the source
    /// before rewriting the lock.)
    #[tokio::test]
    async fn lock_is_committed_before_source_removal() {
        let color_mode = crate::output_style::ColorMode::Plain;
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path();
        BackendLock::local_default().save(base).unwrap();
        local_backend_for_test(base.join(LocalBackend::DEFAULT_STATE_FILE))
            .write_state(&state_with("lin-z", 1))
            .await
            .unwrap();
        let dst_path = base.join("moved.state.json");
        let cfg = carina_core::parser::BackendConfig {
            backend_type: "local".to_string(),
            attributes: {
                let mut m = HashMap::new();
                m.insert(
                    "path".to_string(),
                    Value::Concrete(ConcreteValue::String(
                        dst_path.to_string_lossy().into_owned().into(),
                    )),
                );
                m
            },
            attribute_spans: HashMap::new(),
        };

        run_init_migrate_state(color_mode, base, Some(&cfg), false)
            .await
            .unwrap();

        // The lock on disk is the *configured* lock: the commit happened.
        let on_disk = BackendLock::load(base).unwrap().unwrap();
        let expected = BackendLock::for_config(Some(&cfg)).unwrap();
        assert_eq!(
            on_disk, expected,
            "lock must be rewritten to the configured backend on commit"
        );

        // Source is gone; a re-run must NOT wedge — it sees the
        // committed lock and is a clean no-op.
        assert!(!base.join(LocalBackend::DEFAULT_STATE_FILE).exists());
        let again = run_init_migrate_state(color_mode, base, Some(&cfg), false)
            .await
            .unwrap();
        assert_eq!(again, MigrationOutcome::NotNeeded);
    }
}
