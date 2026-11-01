//! A project alias must not move filesystem IO after apply has pinned its tree.
#![cfg(unix)]

use std::fs;
use std::os::unix::fs::symlink;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

fn command() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_carina"));
    command.env("NO_COLOR", "1").env_remove("CLICOLOR_FORCE");
    command
}

fn success(output: &Output) {
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn project(path: &Path, marker: &str) {
    fs::create_dir(path).unwrap();
    let upstream = path.join("upstream");
    fs::create_dir(&upstream).unwrap();
    fs::write(
        upstream.join("main.crn"),
        format!("exports {{ marker: String = \"{marker}\" }}\n"),
    )
    .unwrap();
    success(&command().arg("init").arg(&upstream).output().unwrap());
    success(
        &command()
            .arg("apply")
            .arg(&upstream)
            .arg("--auto-approve")
            .output()
            .unwrap(),
    );
    fs::create_dir(path.join("module")).unwrap();
    fs::write(
        path.join("module/main.crn"),
        r#"arguments { name: String }
let item = mock.test.resource {
  name = name
  comment = secret("PINNED_TREE_SECRET")
}
"#,
    )
    .unwrap();
    fs::write(
        path.join("main.crn"),
        format!(
            r#"backend local {{ path = "carina.state.json" }}
provider mock {{}}
let upstream = upstream_state {{ source = "./upstream" }}
let module = use {{ source = "./module" }}
let instance = module {{ name = upstream.marker }}
exports {{ marker: String = "{marker}" }}
"#
        ),
    )
    .unwrap();
    success(&command().arg("init").arg(path).output().unwrap());
}

fn retarget_during_apply(saved: bool) {
    let temp = tempfile::tempdir().unwrap();
    let original = temp.path().join("original");
    let replacement = temp.path().join("replacement");
    let alias = temp.path().join("project-alias");
    project(&original, "original");
    project(&replacement, "replacement");
    symlink(&original, &alias).unwrap();

    let plan = temp.path().join("plan.json");
    if saved {
        success(
            &command()
                .env("CARINA_MOCK_STATE_FILE", temp.path().join("mock.json"))
                .arg("plan")
                .arg(&alias)
                .arg("--out")
                .arg(&plan)
                .output()
                .unwrap(),
        );
        // Both config and state in the new tree are unsuitable for this plan.
        // A pinned apply must never consult either of them after the check.
        fs::write(
            replacement.join("main.crn"),
            "backend local { path = \"wrong-state.json\" }\n",
        )
        .unwrap();
    }
    let sentinel = b"state in the replacement tree must stay untouched";
    fs::write(replacement.join("carina.state.json"), sentinel).unwrap();

    let handshake = tempfile::tempdir().unwrap();
    let variable = if saved {
        "CARINA_TEST_SAVED_PLAN_SOURCE_HANDSHAKE_DIR"
    } else {
        "CARINA_TEST_APPLY_LOCK_HANDSHAKE_DIR"
    };
    let mut child = command()
        .arg("apply")
        .arg(if saved { &plan } else { &alias })
        .arg("--auto-approve")
        .env("CARINA_MOCK_STATE_FILE", temp.path().join("mock.json"))
        .env(variable, handshake.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let started = Instant::now();
    while !handshake.path().join("ready").exists() {
        if started.elapsed() >= Duration::from_secs(30) || child.try_wait().unwrap().is_some() {
            let _ = child.kill();
            let output = child.wait_with_output().unwrap();
            panic!("apply did not reach {variable}: {output:?}");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    if !saved {
        assert!(original.join("carina.state.lock").exists());
    }
    fs::remove_file(&alias).unwrap();
    symlink(&replacement, &alias).unwrap();
    fs::write(handshake.path().join("proceed"), b"").unwrap();
    let output = child.wait_with_output().unwrap();
    success(&output);

    let state: serde_json::Value =
        serde_json::from_slice(&fs::read(original.join("carina.state.json")).unwrap()).unwrap();
    if !saved {
        assert_eq!(state["exports"]["marker"], "original");
    }
    assert_eq!(
        state["resources"][0]["current"]["attributes"]["name"],
        "original"
    );
    assert!(original.join("carina-backend.lock").exists());
    assert!(
        !original.join("carina.state.lock").exists(),
        "unlock must use the pinned tree too"
    );
    assert!(!replacement.join("carina.state.lock").exists());
    assert_eq!(
        fs::read(replacement.join("carina.state.json")).unwrap(),
        sentinel
    );
    assert!(!replacement.join("wrong-state.json").exists());
}

#[test]
fn symlink_retarget_after_apply_lock_keeps_state_in_original_tree() {
    retarget_during_apply(false);
}

#[test]
fn symlink_retarget_after_saved_plan_identity_check_keeps_original_config_and_state() {
    retarget_during_apply(true);
}

fn retarget_at_handshake(
    mut command: Command,
    variable: &str,
    alias: &Path,
    replacement: &Path,
) -> Output {
    let handshake = tempfile::tempdir().unwrap();
    let mut child = command
        .env(variable, handshake.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let started = Instant::now();
    while !handshake.path().join("ready").exists() {
        if started.elapsed() >= Duration::from_secs(30) || child.try_wait().unwrap().is_some() {
            let _ = child.kill();
            panic!(
                "command did not reach {variable}: {:?}",
                child.wait_with_output().unwrap()
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    fs::remove_file(alias).unwrap();
    symlink(replacement, alias).unwrap();
    fs::write(handshake.path().join("proceed"), b"").unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn symlink_retarget_after_destroy_config_keeps_original_upstream_and_state() {
    let temp = tempfile::tempdir().unwrap();
    let original = temp.path().join("original");
    let replacement = temp.path().join("replacement");
    let alias = temp.path().join("alias");
    project(&original, "original");
    project(&replacement, "replacement");
    let mock = temp.path().join("mock.json");
    success(
        &command()
            .arg("apply")
            .arg(&original)
            .arg("--auto-approve")
            .env("CARINA_MOCK_STATE_FILE", &mock)
            .output()
            .unwrap(),
    );
    symlink(&original, &alias).unwrap();
    let sentinel = fs::read(replacement.join("carina-backend.lock")).unwrap();
    fs::write(
        replacement.join("upstream/carina.state.json"),
        b"invalid replacement state",
    )
    .unwrap();
    let mut destroy = command();
    destroy
        .arg("destroy")
        .arg(&alias)
        .arg("--auto-approve")
        .env("CARINA_MOCK_STATE_FILE", &mock);
    let output = retarget_at_handshake(
        destroy,
        "CARINA_TEST_DESTROY_CONFIG_HANDSHAKE_DIR",
        &alias,
        &replacement,
    );
    success(&output);
    let state: serde_json::Value =
        serde_json::from_slice(&fs::read(original.join("carina.state.json")).unwrap()).unwrap();
    assert!(state["resources"].as_array().unwrap().is_empty());
    assert!(!original.join("carina.state.lock").exists());
    assert!(!replacement.join("carina.state.json").exists());
    assert_eq!(
        fs::read(replacement.join("carina-backend.lock")).unwrap(),
        sentinel
    );
}

#[test]
fn symlink_retarget_during_init_migrate_state_keeps_lock_and_cleanup_in_original_tree() {
    let temp = tempfile::tempdir().unwrap();
    let original = temp.path().join("original");
    let replacement = temp.path().join("replacement");
    let alias = temp.path().join("alias");
    for directory in [&original, &replacement] {
        fs::create_dir(directory).unwrap();
        fs::write(
            directory.join("main.crn"),
            "exports { marker: String = \"original\" }\n",
        )
        .unwrap();
        success(&command().arg("init").arg(directory).output().unwrap());
        success(
            &command()
                .arg("apply")
                .arg(directory)
                .arg("--auto-approve")
                .output()
                .unwrap(),
        );
    }
    fs::write(
        original.join("main.crn"),
        "backend local { path = \"new-state.json\" }\nexports { marker: String = \"original\" }\n",
    )
    .unwrap();
    let replacement_lock = fs::read(replacement.join("carina-backend.lock")).unwrap();
    let replacement_state = fs::read(replacement.join("carina.state.json")).unwrap();
    symlink(&original, &alias).unwrap();
    let mut migrate = command();
    migrate.arg("init").arg(&alias).arg("--migrate-state");
    success(&retarget_at_handshake(
        migrate,
        "CARINA_TEST_MIGRATE_COMMIT_HANDSHAKE_DIR",
        &alias,
        &replacement,
    ));
    assert!(
        !original.join("carina.state.json").exists(),
        "original source must be cleaned up"
    );
    let state: serde_json::Value =
        serde_json::from_slice(&fs::read(original.join("new-state.json")).unwrap()).unwrap();
    assert_eq!(state["exports"]["marker"], "original");
    assert!(
        String::from_utf8(fs::read(original.join("carina-backend.lock")).unwrap())
            .unwrap()
            .contains("new-state.json")
    );
    assert_eq!(
        fs::read(replacement.join("carina-backend.lock")).unwrap(),
        replacement_lock
    );
    assert_eq!(
        fs::read(replacement.join("carina.state.json")).unwrap(),
        replacement_state
    );
    assert!(!replacement.join("new-state.json").exists());
}

fn retarget_state_command(refresh: bool) {
    let temp = tempfile::tempdir().unwrap();
    let original = temp.path().join("original");
    let replacement = temp.path().join("replacement");
    let alias = temp.path().join("alias");
    project(&original, "original");
    project(&replacement, "replacement");
    let mock = temp.path().join("mock.json");
    if refresh {
        success(
            &command()
                .arg("apply")
                .arg(&original)
                .arg("--auto-approve")
                .env("CARINA_MOCK_STATE_FILE", &mock)
                .output()
                .unwrap(),
        );
    } else {
        fs::write(original.join("carina.state.json"), serde_json::to_vec(&serde_json::json!({
            "version": carina_state::StateFile::CURRENT_VERSION,
            "exports_version": carina_state::PublishedExports::CURRENT_VERSION,
            "serial": 1, "lineage": "forget-retarget", "carina_version": "test",
            "resources": [{ "resource_type": "test.resource", "identity": "original", "provider": "mock",
                "current": { "kind": "create_outcome_unknown", "started_at": "2026-10-30T03:00:00Z" } }],
            "exports": {}
        })).unwrap()).unwrap();
    }
    let sentinel = b"replacement state must not be read or overwritten";
    fs::write(replacement.join("carina.state.json"), sentinel).unwrap();
    symlink(&original, &alias).unwrap();
    let mut state = command();
    state.arg("state");
    let variable = if refresh {
        state.arg("refresh");
        "CARINA_TEST_STATE_REFRESH_CONFIG_HANDSHAKE_DIR"
    } else {
        state.args(["forget", "mock.test.resource", "original", "--auto-approve"]);
        "CARINA_TEST_STATE_FORGET_CONFIG_HANDSHAKE_DIR"
    };
    state.arg(&alias).env("CARINA_MOCK_STATE_FILE", &mock);
    success(&retarget_at_handshake(
        state,
        variable,
        &alias,
        &replacement,
    ));
    let state: serde_json::Value =
        serde_json::from_slice(&fs::read(original.join("carina.state.json")).unwrap()).unwrap();
    assert_eq!(
        state["resources"].as_array().unwrap().len(),
        usize::from(refresh)
    );
    assert_eq!(
        fs::read(replacement.join("carina.state.json")).unwrap(),
        sentinel
    );
    assert!(!original.join("carina.state.lock").exists());
    assert!(!replacement.join("carina.state.lock").exists());
}

#[test]
fn symlink_retarget_after_state_forget_config_keeps_original_backend() {
    retarget_state_command(false);
}

#[test]
fn symlink_retarget_after_state_refresh_config_keeps_original_backend_and_upstreams() {
    retarget_state_command(true);
}
