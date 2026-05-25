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

use colored::Colorize;

#[cfg(test)]
use carina_state::LocalBackend;
use carina_state::{
    BackendLock, StateBackend, StateFile, anchored_local_path, resolve_backend_anchored,
};

use crate::error::AppError;

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

/// Migrate state between two already-constructed backends.
///
/// This is the testable core: it takes the source/target backends and
/// the cleanup policy and performs the read → guard → write → verify →
/// (optional) delete sequence. Lock-file rewriting is the caller's
/// responsibility (it owns `base_dir`).
///
/// `force` allows overwriting a target that already contains a
/// *different* state (different lineage or non-empty resources). Without
/// it, a populated target aborts the migration loudly.
async fn perform_state_migration(
    source: &dyn StateBackend,
    target: &dyn StateBackend,
    force: bool,
) -> Result<StateFile, AppError> {
    let state = source
        .read_state()
        .await
        .map_err(AppError::Backend)?
        .ok_or_else(|| {
            AppError::Config(
                "The locked backend holds no state. There is nothing to migrate; \
                 re-run `carina init` (without --migrate-state) to adopt the new \
                 backend, or revert the backend configuration."
                    .to_string(),
            )
        })?;

    // Target guard: refuse to clobber a populated, differently-lineaged
    // target unless the operator explicitly opted in with --force.
    if let Some(existing) = target.read_state().await.map_err(AppError::Backend)?
        && (existing.lineage != state.lineage || !existing.resources.is_empty())
        && !force
    {
        return Err(AppError::Config(format!(
            "The configured backend already contains state (lineage {}, {} \
             resources). Refusing to overwrite it. If this target is a stale \
             copy you want to replace, re-run with --force; otherwise verify \
             the backend configuration points where you expect.",
            existing.lineage,
            existing.resources.len()
        )));
    }

    target
        .write_state(&state)
        .await
        .map_err(AppError::Backend)?;

    // Verify the copy landed before we rewrite the lock / delete the
    // source — a migration that silently lost resources is the worst
    // possible outcome.
    let roundtrip = target
        .read_state()
        .await
        .map_err(AppError::Backend)?
        .ok_or_else(|| {
            AppError::Config(
                "Failed to read state back from the configured backend after \
                 writing. The migration was aborted before touching the lock \
                 or the source."
                    .to_string(),
            )
        })?;
    if roundtrip.lineage != state.lineage || roundtrip.resources.len() != state.resources.len() {
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
    base_dir: &Path,
    backend_config: Option<&carina_core::parser::BackendConfig>,
    force: bool,
) -> Result<MigrationOutcome, AppError> {
    let configured = BackendLock::for_config(backend_config)?;
    let locked = BackendLock::load(base_dir)
        .map_err(AppError::Backend)?
        .ok_or_else(|| {
            AppError::Config(
                "No backend lock found. Run `carina init` (without \
                 --migrate-state) to initialize the project first."
                    .to_string(),
            )
        })?;

    if locked == configured {
        return Ok(MigrationOutcome::NotNeeded);
    }

    println!(
        "{}",
        "Backend configuration changed; migrating state:"
            .cyan()
            .bold()
    );
    println!("{}", locked.describe_diff(&configured));

    let locked_config = locked.to_state_config();

    // Both addresses are reconstructed from the *locked* / *configured*
    // snapshots, so a relative local `path` must be anchored at the
    // project dir (not the binary's CWD) for `carina init <dir>` invoked
    // from elsewhere.
    let source = resolve_backend_anchored(Some(&locked_config), base_dir)
        .await
        .map_err(AppError::Backend)?;
    let target = resolve_backend_anchored(Some(&configured.to_state_config()), base_dir)
        .await
        .map_err(AppError::Backend)?;

    let state = perform_state_migration(source.as_ref(), target.as_ref(), force).await?;
    println!(
        "  {} copied {} resource(s) to the configured backend",
        "✓".green(),
        state.resources.len()
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
    configured.save(base_dir).map_err(AppError::Backend)?;
    println!("  {} updated backend lock", "✓".green());

    // A local source is deleted after the commit (matches the retired
    // `state migrate`); a remote source is kept as a recoverable backup
    // (remote → remote in this scope). A failure to delete here is *not*
    // fatal: the migration already committed, so a leftover old file is
    // harmless and the next run is a no-op.
    let source = if locked.is_local() {
        let path = anchored_local_path(&locked_config, base_dir);
        if !path.exists() {
            SourceDisposition::Deleted
        } else {
            match std::fs::remove_file(&path) {
                Ok(()) => {
                    println!("  {} deleted source local state file", "✓".green());
                    SourceDisposition::Deleted
                }
                Err(e) => {
                    eprintln!(
                        "  {} migration committed, but the old state file {} \
                         could not be removed ({e}); delete it manually.",
                        "warning:".yellow(),
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
        resources: state.resources.len(),
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
            // Identifier is mandatory for any row that should survive a
            // round-trip through `check_and_migrate` (carina#3266): the
            // read path prunes identifier=None rows as historical
            // artifacts. Production-shaped `state.resources` rows always
            // carry an identifier from the provider's apply result.
            s.resources.push(
                ResourceState::new("s3.Bucket", format!("r{i}"), "aws")
                    .with_identifier(format!("bucket-{i}")),
            );
        }
        s
    }

    #[tokio::test]
    async fn migrates_state_from_source_to_empty_target() {
        let tmp = tempfile::tempdir().unwrap();
        let src_path = tmp.path().join("old.state.json");
        let dst_path = tmp.path().join("new.state.json");

        let src = LocalBackend::with_path(src_path.clone());
        let dst = LocalBackend::with_path(dst_path.clone());
        src.write_state(&state_with("lin-1", 3)).await.unwrap();

        let state = perform_state_migration(&src, &dst, false).await.unwrap();
        assert_eq!(state.resources.len(), 3);

        let migrated = dst.read_state().await.unwrap().unwrap();
        assert_eq!(migrated.lineage, "lin-1");
        assert_eq!(migrated.resources.len(), 3);
    }

    #[tokio::test]
    async fn refuses_populated_target_without_force() {
        let tmp = tempfile::tempdir().unwrap();
        let src = LocalBackend::with_path(tmp.path().join("a.json"));
        let dst = LocalBackend::with_path(tmp.path().join("b.json"));
        src.write_state(&state_with("lin-src", 1)).await.unwrap();
        dst.write_state(&state_with("lin-dst", 2)).await.unwrap();

        let err = perform_state_migration(&src, &dst, false)
            .await
            .unwrap_err();
        assert!(
            matches!(&err, AppError::Config(m) if m.contains("already contains state")),
            "got: {err:?}"
        );
        // Target must be untouched on refusal.
        let dst_state = dst.read_state().await.unwrap().unwrap();
        assert_eq!(dst_state.lineage, "lin-dst");
    }

    #[tokio::test]
    async fn force_overwrites_populated_target() {
        let tmp = tempfile::tempdir().unwrap();
        let src = LocalBackend::with_path(tmp.path().join("a.json"));
        let dst = LocalBackend::with_path(tmp.path().join("b.json"));
        src.write_state(&state_with("lin-src", 1)).await.unwrap();
        dst.write_state(&state_with("lin-dst", 2)).await.unwrap();

        perform_state_migration(&src, &dst, true).await.unwrap();
        let dst_state = dst.read_state().await.unwrap().unwrap();
        assert_eq!(dst_state.lineage, "lin-src");
        assert_eq!(dst_state.resources.len(), 1);
    }

    #[tokio::test]
    async fn errors_when_source_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let src = LocalBackend::with_path(tmp.path().join("a.json"));
        let dst = LocalBackend::with_path(tmp.path().join("b.json"));

        let err = perform_state_migration(&src, &dst, false)
            .await
            .unwrap_err();
        assert!(
            matches!(&err, AppError::Config(m) if m.contains("no state")),
            "got: {err:?}"
        );
    }

    #[tokio::test]
    async fn not_needed_when_lock_matches_configured_local() {
        let tmp = tempfile::tempdir().unwrap();
        // Lock = local_default; configured = None (also local default).
        BackendLock::local_default().save(tmp.path()).unwrap();
        let outcome = run_init_migrate_state(tmp.path(), None, false)
            .await
            .unwrap();
        assert_eq!(outcome, MigrationOutcome::NotNeeded);
    }

    #[tokio::test]
    async fn local_to_local_migration_deletes_source() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path();
        // Locked = local default (carina.state.json under base_dir).
        BackendLock::local_default().save(base).unwrap();
        let src_path = base.join(LocalBackend::DEFAULT_STATE_FILE);
        LocalBackend::with_path(src_path.clone())
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
                        dst_path.to_string_lossy().into_owned(),
                    )),
                );
                m
            },
        };

        let outcome = run_init_migrate_state(base, Some(&cfg), false)
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
        let migrated = LocalBackend::with_path(dst_path)
            .read_state()
            .await
            .unwrap()
            .unwrap();
        assert_eq!(migrated.resources.len(), 2);

        // Lock now reflects the configured backend ⇒ a second run is a no-op.
        let second = run_init_migrate_state(base, Some(&cfg), false)
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
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path();
        BackendLock::local_default().save(base).unwrap();
        LocalBackend::with_path(base.join(LocalBackend::DEFAULT_STATE_FILE))
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
                        dst_path.to_string_lossy().into_owned(),
                    )),
                );
                m
            },
        };

        run_init_migrate_state(base, Some(&cfg), false)
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
        let again = run_init_migrate_state(base, Some(&cfg), false)
            .await
            .unwrap();
        assert_eq!(again, MigrationOutcome::NotNeeded);
    }
}
