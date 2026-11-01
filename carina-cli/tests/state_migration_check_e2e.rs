use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use clap::Parser;

#[derive(Parser)]
struct StateCli {
    #[command(subcommand)]
    command: carina_cli::commands::state::StateCommands,
}

#[test]
fn migration_check_rejects_s3_before_backend_construction() {
    let error = StateCli::try_parse_from([
        "carina-state",
        "migrate",
        "--check",
        ".",
        "--state-url",
        "s3://copied-state/legacy.json",
    ])
    .err()
    .expect("offline migration checks must reject S3 during argument parsing");
    assert_eq!(error.kind(), clap::error::ErrorKind::ValueValidation);
    let message = error.to_string();
    assert!(message.contains("local state file"), "{message}");
    assert!(message.contains("s3://"), "{message}");

    let output = Command::new(env!("CARGO_BIN_EXE_carina"))
        .args([
            "state",
            "migrate",
            "--check",
            ".",
            "--state-url",
            "s3://copied-state/legacy.json",
        ])
        .output()
        .unwrap();
    assert!(!output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("local state file"));
}

fn write_current_state(project: &Path) -> PathBuf {
    fs::write(project.join("main.crn"), "provider mock {}\n").unwrap();
    let state = project.join("current-state.json");
    fs::write(
        &state,
        serde_json::to_vec_pretty(&carina_state::StateFile::new()).unwrap(),
    )
    .unwrap();
    state
}

#[test]
fn migration_check_accepts_file_url_and_bare_path() {
    let temp = tempfile::tempdir().unwrap();
    let state = write_current_state(temp.path());
    for url in [
        state.display().to_string(),
        format!("file://{}", state.display()),
        "current-state.json".to_string(),
    ] {
        let output = run_builtin_check(temp.path(), Path::new(&url));
        assert!(output.status.success(), "{output:?}");
        assert!(String::from_utf8_lossy(&output.stdout).contains("already version"));
    }
}

#[test]
fn migration_check_current_state_still_validates_config_directory() {
    let temp = tempfile::tempdir().unwrap();
    let state = write_current_state(temp.path());
    let missing = temp.path().join("missing-config");
    let output = Command::new(env!("CARGO_BIN_EXE_carina"))
        .current_dir(temp.path())
        .args(["state", "migrate", "--check"])
        .arg(&missing)
        .arg("--state-url")
        .arg(&state)
        .output()
        .unwrap();
    assert!(!output.status.success(), "invalid DIR passed: {output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("missing-config"));
}

#[test]
fn migration_check_current_state_still_validates_provider_schema_flags() {
    let temp = tempfile::tempdir().unwrap();
    let state = write_current_state(temp.path());
    let output = Command::new(env!("CARGO_BIN_EXE_carina"))
        .current_dir(temp.path())
        .args(["state", "migrate", "--check"])
        .arg(temp.path())
        .arg("--state-url")
        .arg(state)
        .args(["--provider-schema", "malformed"])
        .output()
        .unwrap();
    assert!(!output.status.success(), "bad flag passed: {output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("expected KIND=PATH"));
}

fn wasm_mock_path() -> Option<PathBuf> {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).parent()?;
    ["carina_provider_mock.wasm", "carina-provider-mock.wasm"]
        .into_iter()
        .map(|name| workspace.join("target/wasm32-wasip2/debug").join(name))
        .find(|path| path.is_file())
}

fn run_check(project: &Path, provider: &Path, state: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_carina"))
        .current_dir(project)
        .env("NO_COLOR", "1")
        .env_remove("CLICOLOR_FORCE")
        .args(["state", "migrate", "--check"])
        .arg(project)
        .arg("--state-url")
        .arg(state)
        .arg("--provider-schema")
        .arg(format!("mock={}", provider.display()))
        .output()
        .expect("run offline state migration check")
}

fn run_builtin_check(project: &Path, state: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_carina"))
        .current_dir(project)
        .env("NO_COLOR", "1")
        .env_remove("CLICOLOR_FORCE")
        .args(["state", "migrate", "--check"])
        .arg(project)
        .arg("--state-url")
        .arg(state)
        .output()
        .expect("run built-in offline state migration check")
}

#[test]
fn migration_check_is_multifile_offline_and_does_not_rewrite_the_copied_state() {
    let Some(wasm) = wasm_mock_path() else {
        eprintln!(
            "skipping migration-check WASM test: build carina-provider-mock for wasm32-wasip2 first"
        );
        return;
    };
    let temp = tempfile::tempdir().expect("create migration-check project");
    let project = temp.path();
    let provider = project.join("provider.wasm");
    fs::copy(wasm, &provider).expect("copy schema-only provider fixture");
    fs::write(
        project.join("providers.crn"),
        format!(
            "provider mock {{\n  source = \"file://{}\"\n}}\n",
            provider.display()
        ),
    )
    .expect("write provider declaration");
    fs::write(
        project.join("sources.crn"),
        "let external = upstream_state { source = \"../not-read-by-check\" }\n",
    )
    .expect("write upstream declaration");
    fs::write(
        project.join("main.crn"),
        "let guarded = mock.test.short_budget { create_mode = \"normal\" }\n",
    )
    .expect("write managed resource");
    fs::write(
        project.join("backend.crn"),
        "backend local { path = \"unused-backend-state.json\" }\n",
    )
    .expect("write backend declaration");

    let state = project.join("copied-v20-state.json");
    let state_bytes = serde_json::to_vec_pretty(&serde_json::json!({
        "version": 20,
        "exports_version": carina_state::PublishedExports::CURRENT_VERSION,
        "serial": 7,
        "lineage": "migration-check-copy",
        "carina_version": "legacy",
        "resources": [{
            "resource_type": "test.short_budget",
            "identity": "guarded",
            "provider": "mock",
            "current": {
                "kind": "applied",
                "identifier": "guarded-provider-id",
                "attributes": {"create_mode": "normal"},
                "nested_write_only": {}
            },
            "binding": "guarded",
            "dependency_bindings": ["external"]
        }],
        "exports": {}
    }))
    .expect("encode v20 state fixture");
    fs::write(&state, &state_bytes).expect("write copied v20 state");

    let output = run_check(project, &provider, &state);
    let rendered = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(
        output.status.success(),
        "migration check failed:\n{rendered}"
    );
    assert!(
        rendered.contains(
            "State dependency migration check passed: 1 row(s) classified, 0 orphan/deposed warning(s); no state was written."
        ),
        "migration check did not report its read-only result:\n{rendered}"
    );
    assert_eq!(
        fs::read(&state).expect("re-read copied state"),
        state_bytes,
        "the check command must never rewrite or reformat its input state"
    );
    assert!(
        !project.join("unused-backend-state.json").exists(),
        "the configured backend must not be opened or written"
    );
}

#[test]
fn migration_check_expands_same_config_for_from_copied_state_without_provider_reads() {
    let temp = tempfile::tempdir().expect("create deferred-loop migration project");
    let project = temp.path();
    fs::write(project.join("providers.crn"), "provider mock {}\n")
        .expect("write built-in provider declaration");
    fs::write(
        project.join("source.crn"),
        "let source = mock.test.deferred_source { name = \"source\" }\n",
    )
    .expect("write state-backed iterable source");
    fs::write(
        project.join("children.crn"),
        r#"let children = for rule in source.generated_rules {
  mock.test.resource { name = rule.action }
}
"#,
    )
    .expect("write deferred managed loop");
    fs::write(
        project.join("consumer.crn"),
        r#"let ready = wait source {
  until      = source.name == "source"
  depends_on = [children]
}

let consumer = mock.test.resource {
  name   = "consumer"
  parent = ready.name
}
"#,
    )
    .expect("write wait and downstream consumer");
    fs::write(
        project.join("backend.crn"),
        "backend local { path = \"unused-backend-state.json\" }\n",
    )
    .expect("write backend declaration");

    let state = project.join("copied-v20-state.json");
    let state_bytes = serde_json::to_vec_pretty(&serde_json::json!({
        "version": 20,
        "exports_version": carina_state::PublishedExports::CURRENT_VERSION,
        "serial": 11,
        "lineage": "migration-check-deferred-loop",
        "carina_version": "legacy",
        "resources": [
            {
                "resource_type": "test.deferred_source",
                "identity": "source",
                "provider": "mock",
                "current": {
                    "kind": "applied",
                    "identifier": "source-provider-id",
                    "attributes": {
                        "name": "source",
                        "generated_rules": [{"action": "child-one"}]
                    },
                    "nested_write_only": {}
                },
                "binding": "source"
            },
            {
                "resource_type": "test.resource",
                "identity": "child-one",
                "provider": "mock",
                "current": {
                    "kind": "applied",
                    "identifier": "child-provider-id",
                    "attributes": {"name": "child-one"},
                    "nested_write_only": {}
                },
                "binding": "children[0]",
                "dependency_bindings": ["source"]
            },
            {
                "resource_type": "test.resource",
                "identity": "consumer",
                "provider": "mock",
                "current": {
                    "kind": "applied",
                    "identifier": "consumer-provider-id",
                    "attributes": {"name": "consumer", "parent": "source"},
                    "nested_write_only": {}
                },
                "binding": "consumer",
                "dependency_bindings": ["ready"]
            }
        ],
        "exports": {}
    }))
    .expect("encode deferred-loop v20 state fixture");
    fs::write(&state, &state_bytes).expect("write deferred-loop copied state");

    let output = run_builtin_check(project, &state);
    let rendered = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(
        output.status.success(),
        "migration check failed:\n{rendered}"
    );
    assert!(
        rendered.contains("0 orphan/deposed warning(s); no state was written."),
        "the wait/group projection must classify without orphan fallback:\n{rendered}"
    );
    assert_eq!(
        fs::read(&state).expect("re-read deferred-loop state"),
        state_bytes,
        "state-backed expansion must remain read-only"
    );
    assert!(!project.join("unused-backend-state.json").exists());
}

#[test]
fn migration_check_reports_rows_that_require_data_source_reads_as_offline_unclassifiable() {
    let temp = tempfile::tempdir().expect("create read-dependent migration project");
    let project = temp.path();
    fs::write(project.join("providers.crn"), "provider mock {}\n")
        .expect("write built-in provider declaration");
    fs::write(
        project.join("source.crn"),
        r#"let roles = read mock.iam.Roles { name_regex = "x" }
"#,
    )
    .expect("write data source");
    fs::write(
        project.join("children.crn"),
        r#"let children = for name in roles.names {
  mock.test.resource { name = name }
}
"#,
    )
    .expect("write read-dependent loop");

    let state = project.join("copied-v20-state.json");
    let state_bytes = serde_json::to_vec_pretty(&serde_json::json!({
        "version": 20,
        "exports_version": carina_state::PublishedExports::CURRENT_VERSION,
        "serial": 12,
        "lineage": "migration-check-read-dependent-loop",
        "carina_version": "legacy",
        "resources": [{
            "resource_type": "test.resource",
            "identity": "children[0]",
            "provider": "mock",
            "current": {
                "kind": "applied",
                "identifier": "child-provider-id",
                "attributes": {"name": "child-one"},
                "nested_write_only": {}
            },
            "binding": "children[0]",
            "dependency_bindings": ["gone_dependency"]
        }],
        "exports": {}
    }))
    .expect("encode read-dependent v20 state fixture");
    fs::write(&state, &state_bytes).expect("write copied v20 state");

    let output = run_builtin_check(project, &state);
    let rendered = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(
        !output.status.success(),
        "offline check must not classify a row whose configured status requires a provider read:\n{rendered}",
    );
    assert!(
        rendered.contains("children[0]")
            && rendered.contains("cannot classify offline: requires reads"),
        "offline check must explain the unclassifiable row explicitly:\n{rendered}",
    );
    assert_eq!(
        fs::read(&state).expect("re-read copied state"),
        state_bytes,
        "the failed offline check must remain read-only",
    );
}

#[test]
fn migration_check_classifies_explicitly_moved_row_as_configured() {
    let temp = tempfile::tempdir().expect("create moved migration-check project");
    let project = temp.path();
    fs::write(project.join("providers.crn"), "provider mock {}\n")
        .expect("write built-in provider declaration");
    fs::write(
        project.join("main.crn"),
        r#"let renamed = mock.test.resource { name = "renamed" }

moved {
  from = mock.test.resource 'old'
  to = mock.test.resource 'renamed'
}
"#,
    )
    .expect("write moved resource configuration");
    fs::write(
        project.join("backend.crn"),
        "backend local { path = \"unused-backend-state.json\" }\n",
    )
    .expect("write backend declaration");

    let state = project.join("copied-v20-state.json");
    fs::write(
        &state,
        serde_json::to_vec_pretty(&serde_json::json!({
            "version": 20,
            "exports_version": carina_state::PublishedExports::CURRENT_VERSION,
            "serial": 3,
            "lineage": "migration-check-moved-row",
            "carina_version": "legacy",
            "resources": [{
                "resource_type": "test.resource",
                "identity": "old",
                "provider": "mock",
                "current": {
                    "kind": "applied",
                    "identifier": "old-provider-id",
                    "attributes": {"name": "renamed"},
                    "nested_write_only": {}
                },
                "binding": "old",
                "dependency_bindings": ["gone_dependency"]
            }],
            "exports": {}
        }))
        .expect("encode moved v20 state"),
    )
    .expect("write moved v20 state");

    let output = run_builtin_check(project, &state);
    let rendered = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    assert!(
        !output.status.success(),
        "the moved row is configured and its missing dependency must be strict:\n{rendered}"
    );
    assert!(
        rendered.contains("gone_dependency") && rendered.contains("dependency_bindings"),
        "check must report the same configured-row migration error as apply:\n{rendered}"
    );
}

#[test]
fn migration_check_classifies_reconciled_legacy_loop_row_as_configured() {
    let temp = tempfile::tempdir().expect("create legacy-loop migration-check project");
    let project = temp.path();
    fs::write(project.join("providers.crn"), "provider mock {}\n")
        .expect("write built-in provider declaration");
    fs::write(
        project.join("main.crn"),
        "for name in ['stable'] {\n  mock.test.resource { name = name }\n}\n",
    )
    .expect("write anonymous loop configuration");

    let state = project.join("copied-v20-state.json");
    fs::write(
        &state,
        serde_json::to_vec_pretty(&serde_json::json!({
            "version": 20,
            "exports_version": carina_state::PublishedExports::CURRENT_VERSION,
            "serial": 4,
            "lineage": "migration-check-legacy-loop-row",
            "carina_version": "legacy",
            "resources": [{
                "resource_type": "test.resource",
                "identity": "_for0[0]",
                "provider": "mock",
                "current": {
                    "kind": "applied",
                    "identifier": "legacy-loop-provider-id",
                    "attributes": {"name": "stable"},
                    "nested_write_only": {}
                },
                "binding": "_for0[0]",
                "dependency_bindings": ["gone_dependency"]
            }],
            "exports": {}
        }))
        .expect("encode legacy-loop v20 state"),
    )
    .expect("write legacy-loop v20 state");

    let output = run_builtin_check(project, &state);
    let rendered = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    assert!(
        !output.status.success(),
        "the reconciled loop row is configured and must be strict:\n{rendered}"
    );
    assert!(
        rendered.contains("gone_dependency") && rendered.contains("dependency_bindings"),
        "check must use the same late anonymous move as apply:\n{rendered}"
    );
}
