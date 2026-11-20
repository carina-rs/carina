use super::TestCommandRuntime;
use carina_core::{
    parser::ProviderContext,
    resource::{PersistedDestroyDependencies, ResolvedResourceId, ResourceId, ResourceIdentity},
    shutdown::ShutdownToken,
};
use carina_provider_mock::{MockBehavior, MockScope};
use carina_state::{ResourceState, StateFile};
use std::{fs, num::NonZeroUsize};

const EXPECTED: [&str; 6] = ["delta", "gamma", "theta", "zulu", "beta", "alpha"];

fn id(name: &str) -> ResolvedResourceId {
    ResolvedResourceId::new(ResourceId::with_provider_identity(
        "mock",
        "test.resource",
        ResourceIdentity::try_from(name.to_owned()).unwrap(),
        None,
    ))
}

#[tokio::test]
async fn issue_4491_state_refresh_orphan_sections() {
    orphan_command_output(
        "state-refresh",
        "test_support::ordering::issue_4491_state_refresh_orphan_sections",
    )
    .await;
}

#[tokio::test]
async fn issue_4491_destroy_cached_orphan_guards() {
    orphan_command_output(
        "destroy-cached",
        "test_support::ordering::issue_4491_destroy_cached_orphan_guards",
    )
    .await;
}

#[tokio::test]
async fn issue_4491_destroy_refreshed_orphan_guards_and_progress() {
    orphan_command_output(
        "destroy-refreshed",
        "test_support::ordering::issue_4491_destroy_refreshed_orphan_guards_and_progress",
    )
    .await;
}

async fn orphan_command_output(mode: &str, test_name: &str) {
    // A libtest subprocess captures the production println!/eprintln! paths
    // without redirecting process-wide descriptors in a parallel test runner.
    if std::env::var("CARINA_4491_COMMAND_OUTPUT").is_err() {
        for _ in 0..8 {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", test_name, "--nocapture"])
                .env("CARINA_4491_COMMAND_OUTPUT", mode)
                .output()
                .unwrap();
            let stdout = String::from_utf8(output.stdout).unwrap();
            let stderr = String::from_utf8(output.stderr).unwrap();
            assert!(output.status.success(), "{mode}: {stdout}\n{stderr}");
            let names: Vec<_> = if mode == "state-refresh" {
                stdout
                    .lines()
                    .filter_map(|line| {
                        line.strip_prefix("  mock.test.resource \"")
                            .and_then(|line| line.strip_suffix("\" (orphan):"))
                    })
                    .collect()
            } else {
                stderr
                    .lines()
                    .filter_map(|line| line.trim().strip_prefix("✗ mock.test.resource."))
                    .collect()
            };
            assert_eq!(names, EXPECTED, "{mode}: {stdout}\n{stderr}");
            if mode == "destroy-refreshed" {
                let mut progress: Vec<_> = stdout
                    .lines()
                    .filter_map(|line| {
                        line.trim()
                            .strip_prefix("✓ mock.test.resource ")
                            .and_then(|line| line.split_once(" took ").map(|(name, _)| name))
                    })
                    .collect();
                // These lines describe completions inside concurrent reads.
                // Their membership is fixed; FuturesUnordered does not promise
                // completion order. The guard/result rows above remain ordered.
                progress.sort_unstable();
                let mut expected = EXPECTED;
                expected.sort_unstable();
                assert_eq!(progress, expected, "completed orphan refreshes");
            }
        }
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("main.crn"), "provider mock {}\n").unwrap();
    let mut state = StateFile::new();
    let mut store = serde_json::Map::new();
    for name in ["alpha", "theta", "beta", "delta", "zulu", "gamma"] {
        let mut row = ResourceState::try_applied("test.resource", name, "mock", name).unwrap();
        row.set_persisted_attribute("name", serde_json::json!(name));
        row.directives.set_prevent_destroy(mode != "state-refresh");
        let dependency = match name {
            "zulu" => Some("beta"),
            "beta" => Some("alpha"),
            _ => None,
        };
        let dependencies = PersistedDestroyDependencies::decode_persisted(
            dependency.map(id).into_iter().collect(),
        )
        .unwrap();
        if name == "beta" {
            row.directives.depends_on = dependencies;
        } else {
            row.dependency_bindings = dependencies;
        }
        state.upsert_resource(row).unwrap();
        store.insert(name.to_owned(), serde_json::json!({"name": name}));
    }
    fs::write(
        dir.path().join("carina.state.json"),
        serde_json::to_vec(&state).unwrap(),
    )
    .unwrap();
    if mode == "state-refresh" {
        store.clear();
    }
    let store_path = dir.path().join("mock.json");
    fs::write(
        &store_path,
        serde_json::to_vec(&serde_json::json!({"version": 3, "states": store})).unwrap(),
    )
    .unwrap();
    let runtime = TestCommandRuntime::with_mock(
        MockBehavior::default(),
        MockScope::file(store_path).unwrap(),
    );
    let project = carina_core::paths::ResolvedPath::new(dir.path()).unwrap();
    crate::commands::ensure_backend_lock(
        &project,
        None::<&carina_core::parser::ExpandedBackendConfig>,
    )
    .unwrap();
    let context = ProviderContext::new(carina_core::EnvironmentSnapshot::empty());
    if mode == "state-refresh" {
        crate::commands::state::run_state_refresh(
            &runtime,
            dir.path(),
            true,
            &context,
            ShutdownToken::running(),
        )
        .await
        .unwrap();
    } else {
        let error = crate::commands::destroy::run_destroy(
            &runtime,
            dir.path(),
            true,
            true,
            mode == "destroy-refreshed",
            false,
            NonZeroUsize::new(1).unwrap(),
            &context,
            ShutdownToken::running(),
        )
        .await
        .unwrap_err();
        assert!(
            error.to_string().contains("6 resource(s) are protected"),
            "{error}"
        );
    }
}
