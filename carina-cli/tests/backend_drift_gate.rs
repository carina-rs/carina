//! End-to-end coverage for moving the backend-drift gate downstream
//! from `init` to mutating commands (carina#3405).

use std::fs;
use std::process::{Command, Output};

use carina_cli::commands::plan::PlanFile;
use carina_core::effect::UnresolvedSavedEffect;
use carina_core::resource::{ConcreteValue, Value};
use carina_state::{BackendLock, LockInfo, ResourceState, StateFile};
use tempfile::TempDir;

fn carina(project: &std::path::Path, args: &[&str]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_carina"));
    cmd.current_dir(project)
        .env("NO_COLOR", "1")
        .env_remove("CLICOLOR_FORCE")
        .env(
            "CARINA_MOCK_STATE_FILE",
            project.join("mock-provider-state.json"),
        )
        .args(args)
        .output()
        .expect("failed to execute carina")
}

fn local_backend_lock(path: &str) -> BackendLock {
    let mut attributes = std::collections::HashMap::new();
    attributes.insert(
        "path".to_string(),
        Value::Concrete(ConcreteValue::String(path.to_string().into())),
    );
    let config: carina_core::parser::ExpandedBackendConfig = carina_core::parser::BackendConfig {
        backend_type: "local".to_string(),
        attributes,
        attribute_spans: Default::default(),
    };
    BackendLock::for_config(Some(&config)).unwrap()
}

fn s3_backend_lock(bucket: &str, key: &str) -> BackendLock {
    let mut attributes = std::collections::HashMap::new();
    attributes.insert(
        "bucket".to_string(),
        Value::Concrete(ConcreteValue::String(bucket.to_string().into())),
    );
    attributes.insert(
        "key".to_string(),
        Value::Concrete(ConcreteValue::String(key.to_string().into())),
    );
    attributes.insert(
        "region".to_string(),
        Value::Concrete(ConcreteValue::String("us-east-1".into())),
    );
    let config: carina_core::parser::ExpandedBackendConfig = carina_core::parser::BackendConfig {
        backend_type: "s3".to_string(),
        attributes,
        attribute_spans: Default::default(),
    };
    BackendLock::for_config(Some(&config)).unwrap()
}

fn state_json(lineage: &str) -> String {
    let mut state = StateFile::new();
    state.serial = 3;
    state.lineage = lineage.to_string();
    state
        .upsert_resource(
            ResourceState::try_applied("test.resource", "r1", "mock", "mock-id")
                .expect("fixture identifier is non-empty")
                .with_attribute("name", serde_json::json!("r1")),
        )
        .expect("test state setup must be valid");
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
    write_drift_project(project, include_resource);

    tmp
}

fn write_drift_project(project: &std::path::Path, include_resource: bool) {
    fs::create_dir_all(project).unwrap();
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
}

fn write_clean_fixture(include_resource: bool) -> TempDir {
    let tmp = TempDir::new().unwrap();
    let project = tmp.path();
    write_project(project, "state.json", include_resource);
    local_backend_lock("state.json").save(project).unwrap();
    tmp
}

fn write_applied_read_drift_fixture() -> TempDir {
    let tmp = TempDir::new().unwrap();
    let project = tmp.path();
    fs::write(
        project.join("backend.crn"),
        "backend local { path = \"state-a.json\" }\nprovider mock {}\n",
    )
    .unwrap();
    fs::write(
        project.join("main.crn"),
        r#"let from_a = mock.test.resource {
  name = "resource-from-a"
}

exports {
  from_a_name: String = from_a.name
}
"#,
    )
    .unwrap();

    let init = carina(project, &["init", "."]);
    assert!(
        init.status.success(),
        "fixture init failed.\nstdout: {}\nstderr: {}",
        stdout(&init),
        stderr(&init),
    );
    let apply = carina(project, &["apply", "--auto-approve", "--lock=false", "."]);
    assert!(
        apply.status.success(),
        "fixture apply failed.\nstdout: {}\nstderr: {}",
        stdout(&apply),
        stderr(&apply),
    );
    assert!(project.join("state-a.json").is_file());

    fs::write(
        project.join("backend.crn"),
        "backend local { path = \"state-b.json\" }\nprovider mock {}\n",
    )
    .unwrap();
    assert!(!project.join("state-b.json").exists());

    tmp
}

fn write_upstream_drift_fixture() -> (TempDir, std::path::PathBuf, std::path::PathBuf) {
    let tmp = TempDir::new().unwrap();
    let upstream = tmp.path().join("upstream");
    let downstream = tmp.path().join("downstream");
    fs::create_dir_all(&upstream).unwrap();
    fs::create_dir_all(&downstream).unwrap();

    fs::write(
        upstream.join("backend.crn"),
        "backend local { path = \"state-b.json\" }\n",
    )
    .unwrap();
    fs::write(
        upstream.join("main.crn"),
        "exports { resource_name: String = \"declared-shape\" }\n",
    )
    .unwrap();
    let mut upstream_state = StateFile::new();
    upstream_state.insert_export_json_for_test(
        "resource_name",
        serde_json::json!("from-upstream-backend-a"),
    );
    fs::write(
        upstream.join("state-a.json"),
        serde_json::to_string_pretty(&upstream_state).unwrap(),
    )
    .unwrap();
    local_backend_lock("state-a.json").save(&upstream).unwrap();

    fs::write(
        downstream.join("backend.crn"),
        "backend local { path = \"downstream.state.json\" }\nprovider mock {}\n",
    )
    .unwrap();
    fs::write(
        downstream.join("main.crn"),
        r#"let upstream = upstream_state {
  source = "../upstream"
}

let from_upstream = mock.test.resource {
  name = upstream.resource_name
}
"#,
    )
    .unwrap();
    local_backend_lock("downstream.state.json")
        .save(&downstream)
        .unwrap();

    (tmp, upstream, downstream)
}

fn assert_read_drift_warning(output: &Output) {
    let stderr = stderr(output);
    assert!(
        stderr.contains("Backend configuration changed")
            && stderr.contains("reads state from the OLD backend recorded in carina-backend.lock")
            && stderr.contains("carina init --migrate-state"),
        "read command must explain that it selected the locked old backend.\nstderr:\n{stderr}",
    );
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

        let output = carina(project, &["init"]);

        assert!(
            output.status.success(),
            "init should warn and exit 0 on drift.\nstdout: {}\nstderr: {}",
            stdout(&output),
            stderr(&output),
        );
        let stderr = stderr(&output);
        let stdout = stdout(&output);
        assert!(
            stderr.contains("Backend configuration changed")
                && stderr.contains("carina init --migrate-state"),
            "init drift warning must name the pending migration, got:\n{stderr}",
        );
        assert!(
            !stderr.contains("carina init --migrate-state ."),
            "bare init must not add a redundant project path, got:\n{stderr}",
        );
        assert!(
            stdout.contains("carina init --migrate-state")
                && !stdout.contains("carina init --migrate-state ."),
            "bare init migration note must omit a redundant project path, got:\n{stdout}",
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
    fn plan_from_different_cwd_includes_explicit_project_path_in_drift_hints() {
        let tmp = TempDir::new().unwrap();
        let project_arg = "infra/foo";
        let project = tmp.path().join(project_arg);
        write_drift_project(&project, true);

        let output = carina(tmp.path(), &["plan", "--refresh=false", project_arg]);

        assert!(
            output.status.success(),
            "plan should warn and exit 0 on drift.\nstdout: {}\nstderr: {}",
            stdout(&output),
            stderr(&output),
        );
        let expected = "carina init --migrate-state infra/foo";
        assert!(
            stderr(&output).contains(expected),
            "stderr drift warning must include the explicit project path.\nstderr:\n{}",
            stderr(&output),
        );
        assert!(
            stdout(&output).contains(expected),
            "stdout migration note must include the explicit project path.\nstdout:\n{}",
            stdout(&output),
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
            stdout.contains("carina init --migrate-state"),
            "migration-pending note must name the migration command, got stdout:\n{stdout}",
        );
        assert!(
            !stdout.contains("carina init --migrate-state ."),
            "migration-pending note must not include a redundant path argument, got stdout:\n{stdout}",
        );
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
    fn state_list_warns_and_uses_locked_backend() {
        let tmp = write_applied_read_drift_fixture();
        let output = carina(tmp.path(), &["state", "list", "."]);

        assert!(
            output.status.success(),
            "state list should read backend A while migration to B is pending.\nstdout: {}\nstderr: {}",
            stdout(&output),
            stderr(&output),
        );
        assert!(
            stdout(&output).contains("mock.test.resource from_a"),
            "state list did not list the resource applied to backend A.\nstdout:\n{}",
            stdout(&output),
        );
        assert_read_drift_warning(&output);
    }

    #[test]
    fn state_show_warns_and_uses_locked_backend() {
        let tmp = write_applied_read_drift_fixture();
        let output = carina(tmp.path(), &["state", "show", "--json", "."]);

        assert!(
            output.status.success(),
            "state show should read backend A while migration to B is pending.\nstdout: {}\nstderr: {}",
            stdout(&output),
            stderr(&output),
        );
        let shown: serde_json::Value =
            serde_json::from_slice(&output.stdout).expect("state show --json must emit valid JSON");
        assert_eq!(shown["resources"][0]["binding"], "from_a");
        assert_read_drift_warning(&output);
    }

    #[test]
    fn state_lookup_warns_and_uses_locked_backend() {
        let tmp = write_applied_read_drift_fixture();
        let output = carina(
            tmp.path(),
            &["state", "lookup", "from_a.name", "--json", "."],
        );

        assert!(
            output.status.success(),
            "state lookup should read backend A while migration to B is pending.\nstdout: {}\nstderr: {}",
            stdout(&output),
            stderr(&output),
        );
        assert_eq!(stdout(&output).trim(), r#""resource-from-a""#);
        assert_read_drift_warning(&output);
    }

    #[test]
    fn export_warns_and_uses_locked_backend() {
        let tmp = write_applied_read_drift_fixture();
        let output = carina(tmp.path(), &["export", "from_a_name", "--raw"]);

        assert!(
            output.status.success(),
            "export should read backend A while migration to B is pending.\nstdout: {}\nstderr: {}",
            stdout(&output),
            stderr(&output),
        );
        assert_eq!(stdout(&output).trim(), "resource-from-a");
        assert_read_drift_warning(&output);
    }

    #[test]
    fn downstream_plan_warns_once_and_reads_upstream_locked_backend() {
        let (_tmp, upstream, downstream) = write_upstream_drift_fixture();
        let output = carina(&downstream, &["plan", "--json", "--refresh=false", "."]);

        assert!(
            output.status.success(),
            "downstream plan should read upstream backend A while U's migration to B is pending.\nstdout: {}\nstderr: {}",
            stdout(&output),
            stderr(&output),
        );
        let stdout = stdout(&output);
        let json_start = stdout
            .find('{')
            .expect("JSON plan must follow provider status lines");
        let plan: PlanFile = serde_json::from_str(&stdout[json_start..]).unwrap();
        let created_name = plan.plan.effects().iter().find_map(|effect| match effect {
            UnresolvedSavedEffect::Create(resource) => resource.as_resource().get_attr("name"),
            _ => None,
        });
        assert_eq!(
            created_name,
            Some(&Value::Concrete(ConcreteValue::String(
                "from-upstream-backend-a".into()
            ))),
            "downstream plan must resolve the export stored in U's locked old backend",
        );

        let upstream = upstream.canonicalize().unwrap();
        let warning = format!(
            "upstream_state source {} has a pending backend migration; reading its OLD backend",
            upstream.display()
        );
        let stderr = stderr(&output);
        assert_eq!(
            stderr.matches(&warning).count(),
            1,
            "upstream migration warning must use upstream-specific wording exactly once.\nstderr:\n{stderr}",
        );
        assert!(
            stderr.contains(&format!(
                "carina init --migrate-state {}",
                upstream.display()
            )),
            "upstream warning must include the migration command for U.\nstderr:\n{stderr}",
        );
    }

    #[test]
    fn saved_plan_apply_warns_once_when_it_resolves_same_drifted_upstream_twice() {
        let (_tmp, upstream, downstream) = write_upstream_drift_fixture();
        let plan = carina(
            &downstream,
            &["plan", "--refresh=false", "--out", "plan.json", "."],
        );
        assert!(
            plan.status.success(),
            "fixture plan should succeed.\nstdout: {}\nstderr: {}",
            stdout(&plan),
            stderr(&plan),
        );

        let output = carina(
            &downstream,
            &["apply", "--auto-approve", "--lock=false", "plan.json"],
        );
        assert!(
            output.status.success(),
            "saved-plan apply should succeed.\nstdout: {}\nstderr: {}",
            stdout(&output),
            stderr(&output),
        );

        let upstream = upstream.canonicalize().unwrap();
        let warning = format!(
            "upstream_state source {} has a pending backend migration; reading its OLD backend",
            upstream.display()
        );
        let stderr = stderr(&output);
        assert_eq!(
            stderr.matches(&warning).count(),
            1,
            "saved-plan apply resolves the upstream twice but must warn once per source.\nstderr:\n{stderr}",
        );
    }

    #[test]
    fn downstream_plan_corrupt_upstream_lock_names_source_and_lock_path() {
        let (_tmp, upstream, downstream) = write_upstream_drift_fixture();
        let upstream = upstream.canonicalize().unwrap();
        let lock_path = upstream.join("carina-backend.lock");
        fs::write(&lock_path, "{ definitely not valid json").unwrap();

        let output = carina(&downstream, &["plan", "--refresh=false", "."]);

        assert!(
            !output.status.success(),
            "a corrupt upstream lock must fail closed",
        );
        let stderr = stderr(&output);
        assert!(
            stderr.contains(&format!("upstream_state source {}", upstream.display())),
            "error must identify the upstream source directory.\nstderr:\n{stderr}",
        );
        assert!(
            stderr.contains(&lock_path.display().to_string()),
            "error must identify the exact unreadable lock file.\nstderr:\n{stderr}",
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
        assert!(!stderr.contains("carina init --migrate-state ."));
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
    fn apply_from_different_cwd_includes_explicit_project_path_in_drift_error() {
        let tmp = TempDir::new().unwrap();
        let project_arg = "infra/foo";
        let project = tmp.path().join(project_arg);
        write_drift_project(&project, true);

        let output = carina(tmp.path(), &["apply", "--auto-approve", project_arg]);

        assert!(
            !output.status.success(),
            "apply must refuse on backend drift"
        );
        let stderr = stderr(&output);
        assert!(
            stderr.contains("carina init --migrate-state infra/foo"),
            "apply drift error must include the explicit project path.\nstderr:\n{stderr}",
        );
        assert!(stderr.contains("Cannot apply without first migrating the state"));
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
        let plan_json: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(project.join("plan.json")).expect("read saved plan"),
        )
        .expect("parse saved plan");
        let recorded_source_path = plan_json
            .get("source_path")
            .and_then(serde_json::Value::as_str)
            .expect("saved plan source path");

        write_project(project, "new/state.json", true);
        let output = carina(project, &["apply", "--auto-approve", "plan.json"]);

        assert!(
            !output.status.success(),
            "saved-plan apply must refuse when current backend config drifted",
        );
        let stderr = stderr(&output);
        assert!(
            stderr.contains(&format!(
                "carina init --migrate-state {}",
                recorded_source_path
            )),
            "saved-plan drift hint must use the recorded source path.\nstderr:\n{stderr}",
        );
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
        let lock = LockInfo::for_test("migrate-state");
        let lock_id = lock.id().to_owned();
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
        assert_eq!(
            stderr(&output),
            "Error: Bucket name 'old-bucket' does not match backend configuration bucket \
             'new-bucket'.\n",
            "bucket-delete mismatch must retain its configuration-error CLI rendering",
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
    fn destroy_from_different_cwd_includes_explicit_project_path_in_drift_error() {
        let tmp = TempDir::new().unwrap();
        let project_arg = "infra/foo";
        let project = tmp.path().join(project_arg);
        write_drift_project(&project, true);

        let output = carina(tmp.path(), &["destroy", "--auto-approve", project_arg]);

        assert!(
            !output.status.success(),
            "destroy must refuse on backend drift",
        );
        let stderr = stderr(&output);
        assert!(
            stderr.contains("carina init --migrate-state infra/foo"),
            "destroy drift error must include the explicit project path.\nstderr:\n{stderr}",
        );
        assert!(stderr.contains("Cannot destroy without first migrating the state"));
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
