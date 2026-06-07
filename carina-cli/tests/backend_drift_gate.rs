//! End-to-end coverage for moving the backend-drift gate downstream
//! from `init` to mutating commands (carina#3405).

use std::fs;
use std::process::{Command, Output};

use carina_core::resource::{ConcreteValue, Value};
use carina_state::{BackendLock, LockInfo, ResourceState, StateFile};
use tempfile::TempDir;

fn carina(project: &std::path::Path, args: &[&str]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_carina"));
    cmd.current_dir(project)
        .env("NO_COLOR", "1")
        .env_remove("CLICOLOR_FORCE")
        .args(args)
        .output()
        .expect("failed to execute carina")
}

fn local_backend_lock(path: &str) -> BackendLock {
    let mut attributes = std::collections::HashMap::new();
    attributes.insert(
        "path".to_string(),
        Value::Concrete(ConcreteValue::String(path.to_string())),
    );
    let config = carina_core::parser::BackendConfig {
        backend_type: "local".to_string(),
        attributes,
    };
    BackendLock::for_config(Some(&config)).unwrap()
}

fn s3_backend_lock(bucket: &str, key: &str) -> BackendLock {
    let mut attributes = std::collections::HashMap::new();
    attributes.insert(
        "bucket".to_string(),
        Value::Concrete(ConcreteValue::String(bucket.to_string())),
    );
    attributes.insert(
        "key".to_string(),
        Value::Concrete(ConcreteValue::String(key.to_string())),
    );
    attributes.insert(
        "region".to_string(),
        Value::Concrete(ConcreteValue::String("us-east-1".to_string())),
    );
    let config = carina_core::parser::BackendConfig {
        backend_type: "s3".to_string(),
        attributes,
    };
    BackendLock::for_config(Some(&config)).unwrap()
}

fn state_json(lineage: &str) -> String {
    let mut state = StateFile::new();
    state.serial = 3;
    state.lineage = lineage.to_string();
    state.upsert_resource(
        ResourceState::new("test.resource", "r1", "mock")
            .with_identifier("mock-id")
            .with_attribute("name", serde_json::json!("r1")),
    );
    serde_json::to_string_pretty(&state).expect("serialize state fixture")
}

fn empty_state_json() -> String {
    serde_json::to_string_pretty(&StateFile::new()).expect("serialize empty state fixture")
}

fn write_project(project: &std::path::Path, backend_path: &str, include_resource: bool) {
    fs::write(
        project.join("backend.crn"),
        format!("backend local {{ path = \"{backend_path}\" }}\n"),
    )
    .unwrap();
    let main = if include_resource {
        "let r1 = mock.test.resource { name = \"r1\" }\n"
    } else {
        "exports {\n}\n"
    };
    fs::write(project.join("main.crn"), main).unwrap();
}

fn write_drift_fixture(include_resource: bool) -> TempDir {
    let tmp = TempDir::new().unwrap();
    let project = tmp.path();
    write_project(project, "state.json", include_resource);

    fs::create_dir_all(project.join("legacy")).unwrap();
    fs::write(
        project.join("legacy/state.json"),
        state_json("legacy-lineage"),
    )
    .unwrap();
    local_backend_lock("legacy/state.json")
        .save(project)
        .unwrap();

    tmp
}

fn write_clean_fixture(include_resource: bool) -> TempDir {
    let tmp = TempDir::new().unwrap();
    let project = tmp.path();
    write_project(project, "state.json", include_resource);
    local_backend_lock("state.json").save(project).unwrap();
    tmp
}

fn lock_json(project: &std::path::Path) -> String {
    fs::read_to_string(project.join("carina-backend.lock")).unwrap()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

mod backend_drift_gate {
    use super::*;

    #[test]
    fn init_warns_but_exits_zero_on_drift() {
        let tmp = write_drift_fixture(false);
        let project = tmp.path();
        let before_lock = lock_json(project);

        let output = carina(project, &["init", "."]);

        assert!(
            output.status.success(),
            "init should warn and exit 0 on drift.\nstdout: {}\nstderr: {}",
            stdout(&output),
            stderr(&output),
        );
        let stderr = stderr(&output);
        assert!(
            stderr.contains("Backend configuration changed")
                && stderr.contains("carina init --migrate-state"),
            "init drift warning must name the pending migration, got:\n{stderr}",
        );
        assert_eq!(
            lock_json(project),
            before_lock,
            "init without --migrate-state must not rewrite the backend lock",
        );
    }

    #[test]
    fn init_migrate_state_still_migrates() {
        let tmp = write_drift_fixture(false);
        let project = tmp.path();

        let output = carina(project, &["init", "--migrate-state", "."]);

        assert!(
            output.status.success(),
            "init --migrate-state should succeed.\nstdout: {}\nstderr: {}",
            stdout(&output),
            stderr(&output),
        );
        assert!(
            project.join("state.json").exists(),
            "state must be migrated to the configured backend path",
        );
        let lock = BackendLock::load(project).unwrap().unwrap();
        assert_eq!(lock, local_backend_lock("state.json"));
    }

    #[test]
    fn init_migrate_state_noop_when_backend_unchanged() {
        let tmp = write_clean_fixture(false);
        let project = tmp.path();

        let output = carina(project, &["init", "--migrate-state", "."]);

        assert!(
            output.status.success(),
            "init --migrate-state should succeed when no migration is needed.\nstdout: {}\nstderr: {}",
            stdout(&output),
            stderr(&output),
        );
        assert!(
            stdout(&output).contains(
                "No state migration needed; backend lock already matches the configuration."
            ),
            "init --migrate-state should use the unchanged-backend no-op message.\nstdout:\n{}",
            stdout(&output),
        );
    }

    #[test]
    fn plan_warns_and_uses_locked_backend() {
        let tmp = write_drift_fixture(true);
        let project = tmp.path();

        let output = carina(project, &["plan", "--refresh=false", "."]);

        assert!(
            output.status.success(),
            "plan should warn and exit 0 on drift.\nstdout: {}\nstderr: {}",
            stdout(&output),
            stderr(&output),
        );
        let stderr = stderr(&output);
        assert!(
            stderr.contains("Backend configuration changed")
                && stderr.contains("carina init --migrate-state"),
            "plan drift warning must name the pending migration, got:\n{stderr}",
        );
        let stdout = stdout(&output);
        assert!(
            stdout.contains("No changes. Infrastructure is up-to-date."),
            "plan must read the locked legacy state and see the existing resource.\nstdout:\n{stdout}",
        );
        assert!(
            !stdout.contains("Create mock.test.resource"),
            "plan read from the configured empty backend instead of the locked legacy backend.\nstdout:\n{stdout}",
        );
    }

    #[test]
    fn plan_drift_warning_prints_after_plan_summary() {
        let tmp = write_drift_fixture(true);
        let project = tmp.path();

        let output = carina(project, &["plan", "--refresh=false", "."]);

        assert!(
            output.status.success(),
            "plan should warn and exit 0 on drift.\nstdout: {}\nstderr: {}",
            stdout(&output),
            stderr(&output),
        );
        let stdout = stdout(&output);
        let no_changes_idx = stdout
            .find("No changes. Infrastructure is up-to-date.")
            .expect("plan summary must be present in stdout");
        let warning_idx = stdout
            .find("Backend migration pending: plan read state from the OLD backend")
            .expect("backend migration warning must be present in stdout");
        assert!(
            warning_idx > no_changes_idx,
            "warning must appear after the plan summary, got stdout:\n{stdout}",
        );
    }

    #[test]
    fn plan_out_with_drift_records_locked_backend() {
        let tmp = write_drift_fixture(true);
        let project = tmp.path();
        let plan_path = project.join("plan.json");

        let output = carina(
            project,
            &["plan", "--refresh=false", "--out", "plan.json", "."],
        );

        assert!(
            output.status.success(),
            "plan --out should succeed while warning on drift.\nstdout: {}\nstderr: {}",
            stdout(&output),
            stderr(&output),
        );
        let plan_json: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(plan_path).unwrap()).unwrap();
        assert_eq!(
            plan_json
                .pointer("/backend_config/attributes/path/String")
                .and_then(serde_json::Value::as_str),
            Some("legacy/state.json"),
            "saved plan must record the locked backend that supplied state:\n{plan_json:#}",
        );
    }

    #[test]
    fn apply_refuses_on_drift() {
        let tmp = write_drift_fixture(true);
        let project = tmp.path();
        let before_state = fs::read_to_string(project.join("legacy/state.json")).unwrap();

        let output = carina(project, &["apply", "--auto-approve", "."]);

        assert!(
            !output.status.success(),
            "apply must refuse on backend drift",
        );
        let stderr = stderr(&output);
        assert!(stderr.contains("carina init --migrate-state"));
        assert!(stderr.contains("Cannot apply without first migrating the state"));
        assert!(!stderr.contains("Cannot refresh state without first migrating the state"));
        assert_eq!(
            fs::read_to_string(project.join("legacy/state.json")).unwrap(),
            before_state,
            "refused apply must not modify the locked legacy state",
        );
        assert!(
            !project.join("state.json").exists(),
            "refused apply must not create state at the configured backend path",
        );
    }

    #[test]
    fn apply_plan_file_refuses_on_drift() {
        let tmp = write_clean_fixture(true);
        let project = tmp.path();
        fs::write(project.join("state.json"), state_json("saved-plan-lineage")).unwrap();

        let plan_output = carina(
            project,
            &["plan", "--refresh=false", "--out", "plan.json", "."],
        );
        assert!(
            plan_output.status.success(),
            "initial plan --out should succeed.\nstdout: {}\nstderr: {}",
            stdout(&plan_output),
            stderr(&plan_output),
        );

        write_project(project, "new/state.json", true);
        let output = carina(project, &["apply", "--auto-approve", "plan.json"]);

        assert!(
            !output.status.success(),
            "saved-plan apply must refuse when current backend config drifted",
        );
        let stderr = stderr(&output);
        assert!(stderr.contains("carina init --migrate-state"));
        assert!(stderr.contains("Cannot apply without first migrating the state"));
    }

    #[test]
    fn apply_plan_file_refuses_when_backend_changed_since_plan() {
        let tmp = write_clean_fixture(false);
        let project = tmp.path();
        fs::write(project.join("state.json"), empty_state_json()).unwrap();

        let plan_output = carina(
            project,
            &["plan", "--refresh=false", "--out", "plan.json", "."],
        );
        assert!(
            plan_output.status.success(),
            "initial plan --out should succeed.\nstdout: {}\nstderr: {}",
            stdout(&plan_output),
            stderr(&plan_output),
        );

        write_project(project, "new/state.json", false);
        fs::create_dir_all(project.join("new")).unwrap();
        let migrate_output = carina(project, &["init", "--migrate-state", "."]);
        assert!(
            migrate_output.status.success(),
            "migration to the new backend should succeed.\nstdout: {}\nstderr: {}",
            stdout(&migrate_output),
            stderr(&migrate_output),
        );

        let output = carina(project, &["apply", "--auto-approve", "plan.json"]);

        assert!(
            !output.status.success(),
            "saved-plan apply must refuse when the clean backend changed since plan time",
        );
        assert!(
            stderr(&output).contains("Saved plan backend does not match the current `backend.crn`"),
            "error should explain that the saved plan backend is stale.\nstderr:\n{}",
            stderr(&output),
        );
    }

    #[test]
    fn apply_moved_plan_file_uses_canonical_source_path() {
        let tmp = write_clean_fixture(false);
        let project = tmp.path();

        let plan_output = carina(
            project,
            &["plan", "--refresh=false", "--out", "plan.json", "."],
        );
        assert!(
            plan_output.status.success(),
            "plan --out should succeed.\nstdout: {}\nstderr: {}",
            stdout(&plan_output),
            stderr(&plan_output),
        );

        let plan_json: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(project.join("plan.json")).unwrap()).unwrap();
        let source_path = plan_json
            .get("source_path")
            .and_then(serde_json::Value::as_str)
            .expect("saved plan should include source_path");
        assert!(
            std::path::Path::new(source_path).is_absolute(),
            "saved plan source_path must be absolute so moved plan files still reload the project"
        );

        let moved_dir = project.join("moved");
        fs::create_dir_all(&moved_dir).unwrap();
        fs::rename(project.join("plan.json"), moved_dir.join("plan.json")).unwrap();

        let output = carina(&moved_dir, &["apply", "--auto-approve", "plan.json"]);

        assert!(
            output.status.success(),
            "saved-plan apply should reload the canonical project source even when the plan file moved.\nstdout: {}\nstderr: {}",
            stdout(&output),
            stderr(&output),
        );
    }

    #[test]
    fn state_refresh_refuses_on_drift() {
        let tmp = write_drift_fixture(true);
        let project = tmp.path();
        let before_state = fs::read_to_string(project.join("legacy/state.json")).unwrap();

        let output = carina(project, &["state", "refresh", "."]);

        assert!(
            !output.status.success(),
            "state refresh must refuse on backend drift",
        );
        let stderr = stderr(&output);
        assert!(stderr.contains("carina init --migrate-state"));
        assert!(stderr.contains("Cannot refresh state without first migrating the state"));
        assert!(!stderr.contains("Cannot apply without first migrating the state"));
        assert_eq!(
            fs::read_to_string(project.join("legacy/state.json")).unwrap(),
            before_state,
            "refused state refresh must not modify the locked legacy state",
        );
        assert!(
            !project.join("state.json").exists(),
            "refused state refresh must not create state at the configured backend path",
        );
    }

    #[test]
    fn force_unlock_targets_locked_backend_on_drift() {
        let tmp = write_drift_fixture(false);
        let project = tmp.path();
        let lock = LockInfo::new("migrate-state");
        let lock_id = lock.id.clone();
        let old_lock_path = project.join("legacy/state.lock");
        let new_lock_path = project.join("state.lock");
        fs::write(&old_lock_path, serde_json::to_string_pretty(&lock).unwrap()).unwrap();

        let output = carina(project, &["force-unlock", &lock_id, "."]);

        assert!(
            output.status.success(),
            "force-unlock should target the locked legacy backend on drift.\nstdout: {}\nstderr: {}",
            stdout(&output),
            stderr(&output),
        );
        assert!(
            !old_lock_path.exists(),
            "force-unlock should remove the stale lock from the locked legacy backend",
        );
        assert!(
            !new_lock_path.exists(),
            "force-unlock must not touch/create a lock at the new configured backend path",
        );
    }

    #[test]
    fn bucket_delete_pinned_to_configured_bucket_name() {
        let tmp = TempDir::new().unwrap();
        let project = tmp.path();
        fs::write(
            project.join("backend.crn"),
            "backend s3 { bucket = \"new-bucket\" key = \"state.json\" region = \"us-east-1\" }\n",
        )
        .unwrap();
        fs::write(project.join("main.crn"), "exports {\n}\n").unwrap();
        s3_backend_lock("old-bucket", "state.json")
            .save(project)
            .unwrap();

        let output = carina(
            project,
            &["state", "bucket-delete", "old-bucket", "--force", "."],
        );

        assert!(
            !output.status.success(),
            "bucket-delete should reject the locked old bucket name when configured bucket differs",
        );
        assert!(
            stderr(&output).contains(
                "Bucket name 'old-bucket' does not match backend configuration bucket 'new-bucket'"
            ),
            "bucket-delete should be pinned to the configured bucket guard.\nstderr:\n{}",
            stderr(&output),
        );
    }

    #[test]
    fn destroy_refuses_on_drift() {
        let tmp = write_drift_fixture(true);
        let project = tmp.path();
        let before_state = fs::read_to_string(project.join("legacy/state.json")).unwrap();

        let output = carina(project, &["destroy", "--auto-approve", "."]);

        assert!(
            !output.status.success(),
            "destroy must refuse on backend drift",
        );
        let stderr = stderr(&output);
        assert!(stderr.contains("carina init --migrate-state"));
        assert!(stderr.contains("Cannot destroy without first migrating the state"));
        assert!(!stderr.contains("Cannot apply without first migrating the state"));
        assert_eq!(
            fs::read_to_string(project.join("legacy/state.json")).unwrap(),
            before_state,
            "refused destroy must not modify the locked legacy state",
        );
        assert!(
            !project.join("state.json").exists(),
            "refused destroy must not create state at the configured backend path",
        );
    }

    #[test]
    fn plan_clean_when_no_drift_works_normally() {
        let tmp = write_clean_fixture(true);
        let project = tmp.path();
        fs::write(project.join("state.json"), state_json("clean-lineage")).unwrap();

        let output = carina(project, &["plan", "--refresh=false", "."]);

        assert!(
            output.status.success(),
            "clean plan should succeed.\nstdout: {}\nstderr: {}",
            stdout(&output),
            stderr(&output),
        );
        assert!(
            !stderr(&output).contains("Backend configuration changed"),
            "clean plan must not warn about backend drift",
        );
    }

    #[test]
    fn apply_clean_when_no_drift_works_normally() {
        let tmp = write_clean_fixture(false);
        let project = tmp.path();

        let output = carina(project, &["apply", "--auto-approve", "."]);

        assert!(
            !stderr(&output).contains("Cannot apply without first migrating the state")
                && !stderr(&output).contains("Cannot destroy without first migrating the state")
                && !stderr(&output)
                    .contains("Cannot refresh state without first migrating the state")
                && !stderr(&output).contains("carina init --migrate-state"),
            "clean apply must not be refused by the backend drift gate.\nstdout: {}\nstderr: {}",
            stdout(&output),
            stderr(&output),
        );
    }
}
