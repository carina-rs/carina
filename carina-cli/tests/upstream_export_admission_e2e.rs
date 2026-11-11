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

fn assert_admission_error(output: &Output, export_key: &str) {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "plan must reject present invalid upstream export 'orgs.{export_key}'\nstdout:\n{stdout}\nstderr:\n{stderr}",
    );
    assert!(
        stderr.contains("orgs") && stderr.contains(export_key),
        "admission error must name the upstream binding and export key:\n{stderr}",
    );
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
    assert_admission_error(&run_cli(&downstream, &["plan", "."]), "count");
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
    assert_admission_error(&run_cli(&downstream, &["plan", "."]), "count");
}

#[test]
fn directory_plan_admits_unused_exports_too() {
    let root = tempfile::tempdir().unwrap();
    let (upstream, downstream) = write_fixture(root.path());
    write_exports(
        &upstream,
        serde_json::json!({"count": 73, "unused": "wrong-type"}),
    );
    assert_admission_error(&run_cli(&downstream, &["plan", "."]), "unused");
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
    assert_admission_error(&run_cli(&downstream, &["plan", "."]), "claim");

    exports["claim"] = claim.clone();
    exports["claims"][0]["$enum"]["api_value"] = serde_json::json!("retired");
    write_exports(&upstream, exports.clone());
    let nested = run_cli(&downstream, &["plan", "."]);
    assert_admission_error(&nested, "claims");
    assert!(String::from_utf8_lossy(&nested.stderr).contains("list element 0"));

    exports["claims"][0] = claim;
    exports["claim"]["$enum"]["identity"]["provider"] = serde_json::json!("other");
    write_exports(&upstream, exports);
    assert_admission_error(&run_cli(&downstream, &["plan", "."]), "claim");
    assert!(!upstream.join(".carina/providers").exists());
}
