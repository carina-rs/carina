//! Upstream admission regressions for carina#4304 phase 4a / carina#2630.
//! Both projects are directory fixtures; only persisted exports are tampered.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use carina_state::StateFile;

fn run_cli(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_carina"))
        .current_dir(dir)
        .env("NO_COLOR", "1")
        .env("RUST_LOG", "warn")
        .env_remove("CLICOLOR_FORCE")
        .args(args)
        .output()
        .expect("run carina")
}

fn assert_success(output: &Output, operation: &str) -> String {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "carina {operation} failed\nstdout:\n{stdout}\nstderr:\n{stderr}",
    );
    stdout.into_owned()
}

fn assert_admission_error(output: &Output, export_key: &str, expected: &str, actual_kind: &str) {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "plan must reject present invalid upstream export 'orgs.{export_key}'\nstdout:\n{stdout}\nstderr:\n{stderr}",
    );
    assert!(
        stderr.contains(&format!("upstream export 'orgs.{export_key}'")),
        "admission error must name the upstream binding and export key:\n{stderr}",
    );
    assert!(
        stderr.contains(&format!("expected {expected}, got {actual_kind}")),
        "admission error must name the expected type and actual kind:\n{stderr}"
    );
}

#[cfg(unix)]
#[test]
fn round1_10_admission_assertion_rejects_unrelated_failures() {
    use std::os::unix::process::ExitStatusExt;
    for message in [
        "unrelated failure while resolving orgs count",
        "upstream export 'orgs.count': expected Bool, got string",
        "upstream export 'orgs.count': expected Int, got bool",
    ] {
        let output = Output {
            status: std::process::ExitStatus::from_raw(1 << 8),
            stdout: vec![],
            stderr: message.as_bytes().to_vec(),
        };
        assert!(
            std::panic::catch_unwind(|| assert_admission_error(&output, "count", "Int", "string"))
                .is_err(),
            "an unrelated error or wrong type/kind must not pass the admission assertion"
        );
    }
}

fn write_fixture(root: &Path) -> (PathBuf, PathBuf) {
    let upstream = root.join("upstream");
    let downstream = root.join("downstream");
    for dir in [&upstream, &downstream] {
        fs::create_dir_all(dir).unwrap();
        fs::write(
            dir.join("backend.crn"),
            "backend local { path = 'carina.state.json' }\n",
        )
        .unwrap();
    }
    fs::write(upstream.join("main.crn"), "let authored_count = 7\n").unwrap();
    fs::write(
        upstream.join("exports.crn"),
        "exports {\n  count: Int = authored_count\n  unused: Bool = true\n}\n",
    )
    .unwrap();
    fs::write(downstream.join("providers.crn"), "provider mock {}\n").unwrap();
    // Interpolation accepts either Int or String at the downstream sink.
    // Rejection must come from the upstream's Int contract.
    fs::write(
        downstream.join("main.crn"),
        r#"let orgs = upstream_state { source = "../upstream" }
let consumer = mock.test.resource {
  name = "consumer"
  comment = "${orgs.count}"
}
"#,
    )
    .unwrap();
    assert_success(&run_cli(&downstream, &["init", "."]), "init downstream");
    (upstream, downstream)
}

fn write_exports(upstream: &Path, exports: serde_json::Value) {
    let mut state = serde_json::to_value(StateFile::new()).unwrap();
    state["exports"] = exports;
    fs::write(
        upstream.join("carina.state.json"),
        serde_json::to_vec_pretty(&state).unwrap(),
    )
    .unwrap();
}

fn assert_saved_upstream_change_is_drift(change: impl FnOnce(&Path)) {
    let root = tempfile::tempdir().unwrap();
    let (upstream, downstream) = write_fixture(root.path());
    write_exports(&upstream, serde_json::json!({"count": 73, "unused": true}));
    assert_success(
        &run_cli(&downstream, &["plan", "--out", "saved.plan", "."]),
        "save upstream snapshot",
    );
    change(&upstream);
    let output = run_cli(&downstream, &["apply", "--auto-approve", "saved.plan"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !output.status.success(),
        "changed upstream admission must invalidate the plan\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("upstream_state 'orgs'")
            && stderr.contains("drifted since the plan was created"),
        "upstream admission changes must use the existing drift error\n{stderr}"
    );
    assert!(
        !downstream.join("carina.state.json").exists(),
        "upstream drift must be detected before state writeback"
    );
}

#[test]
fn review_2_saved_apply_detects_removed_export_declaration() {
    assert_saved_upstream_change_is_drift(|upstream| {
        fs::write(
            upstream.join("exports.crn"),
            "exports { count: Int = authored_count }\n",
        )
        .unwrap();
    });
}

#[test]
fn review_2_saved_apply_reports_new_admission_failure_as_drift() {
    assert_saved_upstream_change_is_drift(|upstream| {
        fs::write(
            upstream.join("exports.crn"),
            "exports { count: String = 'changed'\nunused: Bool = true }\n",
        )
        .unwrap();
    });
}

#[test]
fn review_2_saved_apply_allows_known_to_unchecked_admission_change() {
    let root = tempfile::tempdir().unwrap();
    let (upstream, downstream) = write_fixture(root.path());
    write_exports(&upstream, serde_json::json!({"count": 73, "unused": true}));
    assert_success(
        &run_cli(&downstream, &["plan", "--out", "saved.plan", "."]),
        "save upstream snapshot",
    );
    fs::write(upstream.join("main.crn"), "let authored_count = 7\nlet untyped = unavailable.thing { name = 'metadata unavailable' }\n").unwrap();
    fs::write(
        upstream.join("exports.crn"),
        "exports { count: Int = authored_count\nunused = untyped.value }\n",
    )
    .unwrap();
    assert_success(
        &run_cli(&downstream, &["apply", "--auto-approve", "saved.plan"]),
        "apply unchanged values with an unchecked source type",
    );

    let saved: serde_json::Value =
        serde_json::from_slice(&fs::read(downstream.join("saved.plan")).unwrap()).unwrap();
    assert_eq!(saved["version"], serde_json::json!(44));
    let sources = saved["upstream_sources"].as_array().unwrap();
    assert_eq!(sources.len(), 1);
    assert_eq!(
        sources[0]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<std::collections::BTreeSet<_>>(),
        std::collections::BTreeSet::from(["binding", "source"]),
        "format 44 must not acquire source-type evidence fields",
    );
}

#[test]
fn upstream_export_violating_declared_type_is_rejected_by_directory_plan() {
    let root = tempfile::tempdir().unwrap();
    let (upstream, downstream) = write_fixture(root.path());
    write_exports(&upstream, serde_json::json!({"count": 73, "unused": true}));
    assert_success(&run_cli(&downstream, &["plan", "."]), "plan valid control");

    write_exports(
        &upstream,
        serde_json::json!({"count": "stale-string", "unused": true}),
    );
    assert_admission_error(
        &run_cli(&downstream, &["plan", "."]),
        "count",
        "Int",
        "string",
    );
}

#[test]
fn upstream_top_level_null_export_stays_absent_in_directory_plan() {
    let root = tempfile::tempdir().unwrap();
    let (upstream, downstream) = write_fixture(root.path());
    fs::write(
        downstream.join("main.crn"),
        r#"let orgs = upstream_state { source = "../upstream" }
let consumer = mock.test.resource {
  name = "consumer"
  comment = "${orgs.count ?? 901}"
}
"#,
    )
    .unwrap();
    write_exports(&upstream, serde_json::json!({"count": 73, "unused": true}));
    let present = assert_success(&run_cli(&downstream, &["plan", "."]), "plan present export");
    assert!(
        present.contains("comment: '73'"),
        "a present export must win over the coalesce default:\n{present}",
    );

    write_exports(
        &upstream,
        serde_json::json!({"count": null, "unused": true}),
    );
    let absent = assert_success(
        &run_cli(&downstream, &["plan", "."]),
        "plan top-level null export",
    );
    assert!(
        absent.contains("comment: '901'"),
        "a top-level null export must stay absent and select the coalesce default:\n{absent}",
    );
    assert!(
        !absent.contains("known after upstream apply"),
        "an absent export in a loaded snapshot must not become an unknown snapshot:\n{absent}",
    );
}

#[test]
fn valid_upstream_exports_load_unchanged_in_directory_plan() {
    let root = tempfile::tempdir().unwrap();
    let (upstream, downstream) = write_fixture(root.path());
    write_exports(&upstream, serde_json::json!({"count": 73, "unused": false}));
    let state_path = upstream.join("carina.state.json");
    let before = fs::read(&state_path).unwrap();

    let stdout = assert_success(&run_cli(&downstream, &["plan", "."]), "plan valid exports");
    assert!(
        stdout.contains("73"),
        "persisted count must reach the consumer:\n{stdout}"
    );
    assert_eq!(fs::read(&state_path).unwrap(), before);
    assert!(!upstream.join(".carina/providers").exists());
    assert!(!downstream.join("carina.state.json").exists());
}

#[test]
fn missing_upstream_state_still_warns_and_stays_unknown_in_directory_plan() {
    let root = tempfile::tempdir().unwrap();
    let (upstream, downstream) = write_fixture(root.path());
    let output = run_cli(&downstream, &["plan", "."]);
    let stdout = assert_success(&output, "plan missing upstream state");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Warning:") && stderr.contains("orgs") && stderr.contains("no state found"),
        "missing state under Lenient must still warn:\n{stderr}",
    );
    assert!(
        stdout.contains("known after upstream apply: orgs.count"),
        "an omitted upstream binding must remain unknown:\n{stdout}",
    );
    assert!(!upstream.join("carina.state.json").exists());
}

#[test]
fn directory_plan_does_not_read_transitive_upstream_state() {
    let root = tempfile::tempdir().unwrap();
    let (upstream, downstream) = write_fixture(root.path());
    let transitive = root.path().join("transitive");
    fs::create_dir(&transitive).unwrap();
    fs::write(transitive.join("main.crn"), "let seed = 1\n").unwrap();
    fs::write(
        transitive.join("exports.crn"),
        "exports { count: Int = seed }\n",
    )
    .unwrap();
    fs::write(
        transitive.join("backend.crn"),
        "backend local { path = 'poison.state.json' }\n",
    )
    .unwrap();
    let poison = b"not JSON: transitive upstream state must never be read";
    fs::write(transitive.join("poison.state.json"), poison).unwrap();
    fs::write(
        upstream.join("main.crn"),
        "let authored_count = 7\nlet internal = upstream_state { source = '../transitive' }\n",
    )
    .unwrap();
    write_exports(&upstream, serde_json::json!({"count": 73, "unused": true}));

    let stdout = assert_success(
        &run_cli(&downstream, &["plan", "."]),
        "plan transitive upstream",
    );
    assert!(
        stdout.contains("73"),
        "direct upstream must still load:\n{stdout}"
    );
    assert_eq!(
        fs::read(transitive.join("poison.state.json")).unwrap(),
        poison
    );
}

#[test]
fn directory_plan_admits_inferred_exports_against_the_source_type() {
    let root = tempfile::tempdir().unwrap();
    let (upstream, downstream) = write_fixture(root.path());
    fs::write(
        upstream.join("exports.crn"),
        "exports { count = authored_count }\n",
    )
    .unwrap();
    write_exports(&upstream, serde_json::json!({"count": 73}));
    assert_success(
        &run_cli(&downstream, &["plan", "."]),
        "plan inferred export",
    );
    write_exports(&upstream, serde_json::json!({"count": "wrong-type"}));
    assert_admission_error(
        &run_cli(&downstream, &["plan", "."]),
        "count",
        "Int",
        "string",
    );
}

#[test]
fn directory_plan_admits_unused_exports_too() {
    let root = tempfile::tempdir().unwrap();
    let (upstream, downstream) = write_fixture(root.path());
    write_exports(
        &upstream,
        serde_json::json!({"count": 73, "unused": "wrong-type"}),
    );
    assert_admission_error(
        &run_cli(&downstream, &["plan", "."]),
        "unused",
        "Bool",
        "string",
    );
}

#[test]
fn directory_plan_uses_downstream_metadata_for_named_upstream_exports() {
    let root = tempfile::tempdir().unwrap();
    let (upstream, downstream) = write_fixture(root.path());
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let wasm = ["carina_provider_mock.wasm", "carina-provider-mock.wasm"]
        .into_iter()
        .map(|name| workspace.join("target/wasm32-wasip2/debug").join(name))
        .find(|path| path.is_file())
        .expect("build carina-provider-mock for wasm32-wasip2 before this test");
    let source = format!("file://{}", wasm.display());
    fs::write(
        downstream.join("providers.crn"),
        format!("provider mock {{ source = '{source}' }}\n"),
    )
    .unwrap();
    assert_success(
        &run_cli(&downstream, &["init", ".", "--accept-egress", &source]),
        "init downstream with named-type metadata",
    );
    fs::write(
        downstream.join("main.crn"),
        "let orgs = upstream_state { source = '../upstream' }\nexports { selected: Int = orgs.count }\n",
    ).unwrap();
    fs::write(
        upstream.join("exports.crn"),
        "exports { count: Int = authored_count\nclaim: mock.actions.OidcSubject.ClaimKey = mock.actions.OidcSubject.ClaimKey.repo\nclaims: list(mock.actions.OidcSubject.ClaimKey) = [mock.actions.OidcSubject.ClaimKey.repo] }\n",
    ).unwrap();
    let claim = serde_json::json!({"$enum": {
            "identity": {"provider": "mock", "segments": ["actions", "OidcSubject"], "kind": "ClaimKey"},
            "api_value": "repo"
    }});
    let mut exports = serde_json::json!({
        "count": 73,
        "claim": claim.clone(),
        "claims": [claim.clone()]
    });
    write_exports(&upstream, exports.clone());
    assert_success(&run_cli(&downstream, &["plan", "."]), "plan named export");
    exports["claim"]["$enum"]["api_value"] = serde_json::json!("retired");
    write_exports(&upstream, exports.clone());
    let rejected = run_cli(&downstream, &["plan", "."]);
    assert_admission_error(
        &rejected,
        "claim",
        "mock.actions.OidcSubject.ClaimKey",
        "enum",
    );
    assert!(
        !String::from_utf8_lossy(&rejected.stderr).contains("retired"),
        "enum admission must not disclose the upstream value"
    );

    exports["claim"] = claim.clone();
    exports["claims"][0]["$enum"]["api_value"] = serde_json::json!("retired");
    write_exports(&upstream, exports.clone());
    let nested = run_cli(&downstream, &["plan", "."]);
    assert_admission_error(
        &nested,
        "claims",
        "mock.actions.OidcSubject.ClaimKey",
        "enum",
    );
    assert!(String::from_utf8_lossy(&nested.stderr).contains("list element 0"));

    exports["claims"][0] = claim;
    exports["claim"]["$enum"]["identity"]["provider"] = serde_json::json!("other");
    write_exports(&upstream, exports);
    assert_admission_error(
        &run_cli(&downstream, &["plan", "."]),
        "claim",
        "mock.actions.OidcSubject.ClaimKey",
        "enum",
    );
    assert!(!upstream.join(".carina/providers").exists());
}
