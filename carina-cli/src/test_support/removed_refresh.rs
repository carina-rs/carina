use super::TestCommandRuntime;
use crate::commands::{apply, destroy, plan, state};
use carina_core::{parser::ProviderContext, shutdown::ShutdownToken};
use carina_provider_mock::{MockBehavior, MockObservations, MockScope};
use carina_state::{ResourceState, StateFile};
use std::{fs, num::NonZeroUsize, path::PathBuf};

struct Fixture {
    dir: tempfile::TempDir,
    runtime: TestCommandRuntime,
    log: PathBuf,
    routed: bool,
}

impl Fixture {
    fn new() -> Self {
        Self::with_route(false)
    }

    fn with_route(routed: bool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("mock.json");
        fs::write(&store, "invalid mock store").unwrap();
        let log = dir.path().join("operations.log");
        let runtime = TestCommandRuntime::with_mock(
            MockBehavior {
                observations: MockObservations {
                    operation_log: Some(log.clone()),
                    ..Default::default()
                },
                ..Default::default()
            },
            MockScope::file(store).unwrap(),
        );
        let mut state = StateFile::new();
        let mut row = ResourceState::try_applied("test.resource", "old", "mock", "old-id").unwrap();
        row.directives.provider_instance = routed.then(|| "alias".to_owned());
        row.set_persisted_attribute("name", serde_json::json!("old"));
        state.upsert_resource(row).unwrap();
        fs::write(
            dir.path().join("carina.state.json"),
            serde_json::to_vec_pretty(&state).unwrap(),
        )
        .unwrap();
        let fixture = Self {
            dir,
            runtime,
            log,
            routed,
        };
        fixture.config(false);
        crate::commands::ensure_backend_lock(
            fixture.dir.path(),
            None::<&carina_core::parser::ExpandedBackendConfig>,
        )
        .unwrap();
        fixture
    }

    fn config(&self, removed: bool) {
        let provider = if self.routed {
            "provider mock {}\nlet alias = provider mock {}\n"
        } else {
            "provider mock {}\n"
        };
        let claim = if removed {
            "removed { from = mock.test.resource 'old' }\n"
        } else {
            ""
        };
        fs::write(self.dir.path().join("providers.crn"), provider).unwrap();
        fs::write(self.dir.path().join("main.crn"), claim).unwrap();
    }

    async fn plan(
        &self,
        refresh: bool,
        out: Option<&std::path::Path>,
    ) -> Result<bool, crate::error::AppError> {
        plan::run_plan(
            &self.runtime,
            self.dir.path(),
            out,
            None,
            None,
            crate::DetailLevel::None,
            false,
            refresh,
            true,
            false,
            false,
            &ProviderContext::new(carina_core::EnvironmentSnapshot::empty()),
        )
        .await
    }

    async fn apply(&self) -> Result<(), crate::error::AppError> {
        apply::run_apply(
            &self.runtime,
            self.dir.path(),
            true,
            true,
            NonZeroUsize::new(1).unwrap(),
            false,
            &ProviderContext::new(carina_core::EnvironmentSnapshot::empty()),
            ShutdownToken::running(),
        )
        .await
    }

    async fn destroy(&self, refresh: bool) -> Result<(), crate::error::AppError> {
        destroy::run_destroy(
            &self.runtime,
            self.dir.path(),
            true,
            true,
            refresh,
            false,
            NonZeroUsize::new(1).unwrap(),
            &ProviderContext::new(carina_core::EnvironmentSnapshot::empty()),
            ShutdownToken::running(),
        )
        .await
    }

    async fn refresh(&self) -> Result<(), crate::error::AppError> {
        state::run_state_refresh(
            &self.runtime,
            self.dir.path(),
            true,
            &ProviderContext::new(carina_core::EnvironmentSnapshot::empty()),
            ShutdownToken::running(),
        )
        .await
    }

    fn bytes(&self) -> Vec<u8> {
        fs::read(self.dir.path().join("carina.state.json")).unwrap()
    }
    fn assert_empty(&self) {
        let state: serde_json::Value = serde_json::from_slice(&self.bytes()).unwrap();
        assert!(
            state["resources"].as_array().unwrap().is_empty(),
            "removed row must be dropped from state"
        );
    }
    fn clear_log(&self) {
        fs::write(&self.log, "").unwrap();
    }
    fn assert_no_operations(&self) {
        assert_eq!(
            fs::read_to_string(&self.log).unwrap_or_default(),
            "",
            "removed target must never reach the provider"
        );
    }
    fn assert_read_error(error: crate::error::AppError) {
        assert!(
            error.to_string().contains("Failed to load mock state"),
            "fixture must fail with a non-NotFound read error: {error}"
        );
    }
}

#[tokio::test]
async fn removed_no_refresh_plan() {
    let f = Fixture::new();
    Fixture::assert_read_error(f.plan(true, None).await.unwrap_err());
    f.clear_log();
    f.config(true);
    let before = f.bytes();
    let out = f.dir.path().join("plan.json");
    assert!(f.plan(true, Some(&out)).await.unwrap());
    let saved: serde_json::Value = serde_json::from_slice(&fs::read(out).unwrap()).unwrap();
    let effects = saved["plan"]["effects"].as_array().unwrap();
    assert!(saved["current_states"].as_array().unwrap().is_empty());
    assert_eq!(effects.len(), 1);
    assert!(effects[0].get("Remove").is_some(), "{effects:?}");
    assert_eq!(f.bytes(), before);
    f.assert_no_operations();
}

#[tokio::test]
async fn removed_no_refresh_live_apply() {
    let f = Fixture::new();
    Fixture::assert_read_error(f.apply().await.unwrap_err());
    f.clear_log();
    f.config(true);
    f.apply().await.unwrap();
    f.assert_empty();
    f.assert_no_operations();
}

#[tokio::test]
async fn removed_no_refresh_destroy() {
    for refresh in [true, false] {
        let f = Fixture::new();
        Fixture::assert_read_error(f.destroy(true).await.unwrap_err());
        f.clear_log();
        f.config(true);
        f.destroy(refresh).await.unwrap();
        f.assert_empty();
        f.assert_no_operations();
    }
}

#[tokio::test]
async fn removed_no_refresh_state_refresh() {
    let f = Fixture::new();
    Fixture::assert_read_error(f.refresh().await.unwrap_err());
    f.clear_log();
    f.config(true);
    f.refresh().await.unwrap();
    f.assert_empty();
    f.assert_no_operations();
}

#[tokio::test]
async fn removed_no_refresh_saved_plan_apply() {
    let f = Fixture::new();
    Fixture::assert_read_error(f.plan(true, None).await.unwrap_err());
    f.clear_log();
    f.config(true);
    let out = f.dir.path().join("plan.json");
    f.plan(false, Some(&out)).await.unwrap();
    // Exercise historical secret verification of a snapshot recorded by an
    // older producer, including when new plans omit state-only removals.
    let id = carina_core::resource::ResourceId::with_provider_identity(
        "mock",
        "test.resource",
        "old",
        None,
    );
    let mut snapshot =
        carina_core::resource::State::<carina_core::resource::NoPlaceholder>::existing(
            id.clone(),
            std::collections::HashMap::from([(
                "token".into(),
                carina_core::resource::Value::Deferred(
                    carina_core::resource::DeferredValue::Secret(Box::new(
                        carina_core::resource::Value::Concrete(
                            carina_core::resource::ConcreteValue::String(
                                "historical-secret".into(),
                            ),
                        ),
                    )),
                ),
            )]),
        );
    snapshot.identifier = Some("old-id".into());
    let snapshot = carina_core::value::redact_secrets_in_state(&snapshot).unwrap();
    let entry = plan::CurrentStateEntry::Managed {
        id,
        current: plan::SavedManagedCurrent::from_current(
            carina_core::resource::Current::from_snapshot(snapshot),
        ),
    };
    let mut saved: serde_json::Value = serde_json::from_slice(&fs::read(&out).unwrap()).unwrap();
    saved["current_states"] = serde_json::json!([entry]);
    fs::write(&out, serde_json::to_vec_pretty(&saved).unwrap()).unwrap();
    // Conversely, the current DSL's claim cannot exclude an unclaimed saved
    // snapshot. Prove this same historical read fails without a recorded Remove.
    let mut unclaimed = saved.clone();
    unclaimed["plan"]["effects"] = serde_json::json!([]);
    let unclaimed_path = f.dir.path().join("unclaimed.json");
    fs::write(
        &unclaimed_path,
        serde_json::to_vec_pretty(&unclaimed).unwrap(),
    )
    .unwrap();
    let before = f.bytes();
    Fixture::assert_read_error(
        apply::run_apply_from_plan(
            &f.runtime,
            &unclaimed_path,
            true,
            true,
            NonZeroUsize::new(1).unwrap(),
            false,
            &ProviderContext::new(carina_core::EnvironmentSnapshot::empty()),
            ShutdownToken::running(),
        )
        .await
        .unwrap_err(),
    );
    assert_eq!(f.bytes(), before);
    f.clear_log();
    // Saved-plan selection must follow the recorded Remove, even after the
    // current configuration no longer contains the claim.
    f.config(false);
    apply::run_apply_from_plan(
        &f.runtime,
        &out,
        true,
        true,
        NonZeroUsize::new(1).unwrap(),
        false,
        &ProviderContext::new(carina_core::EnvironmentSnapshot::empty()),
        ShutdownToken::running(),
    )
    .await
    .unwrap();
    f.assert_empty();
    f.assert_no_operations();
}

#[tokio::test]
async fn removed_no_refresh_routed_alias_commands() {
    for command in ["plan", "apply", "destroy", "destroy_cached", "refresh"] {
        let f = Fixture::with_route(true);
        Fixture::assert_read_error(f.plan(true, None).await.unwrap_err());
        f.clear_log();
        f.config(true);
        let before = f.bytes();
        match command {
            "plan" => {
                assert!(f.plan(true, None).await.unwrap());
                assert_eq!(f.bytes(), before);
            }
            "apply" => f.apply().await.unwrap(),
            "destroy" => f.destroy(true).await.unwrap(),
            "destroy_cached" => f.destroy(false).await.unwrap(),
            "refresh" => f.refresh().await.unwrap(),
            _ => unreachable!(),
        }
        if command != "plan" {
            f.assert_empty();
        }
        f.assert_no_operations();
    }
}

#[tokio::test]
async fn removed_no_refresh_does_not_hide_an_unclaimed_orphan_error() {
    let f = Fixture::new();
    Fixture::assert_read_error(f.plan(true, None).await.unwrap_err());
    f.clear_log();
    // A real removal claim leaves a sibling orphan eligible for refresh.
    let mut saved: serde_json::Value = serde_json::from_slice(&f.bytes()).unwrap();
    let mut unclaimed = saved["resources"][0].clone();
    unclaimed["identity"] = serde_json::json!("unclaimed");
    unclaimed["current"]["identifier"] = serde_json::json!("unclaimed-id");
    saved["resources"].as_array_mut().unwrap().push(unclaimed);
    fs::write(
        f.dir.path().join("carina.state.json"),
        serde_json::to_vec_pretty(&saved).unwrap(),
    )
    .unwrap();
    f.config(true);
    let before = f.bytes();
    Fixture::assert_read_error(f.plan(true, None).await.unwrap_err());
    Fixture::assert_read_error(f.apply().await.unwrap_err());
    Fixture::assert_read_error(f.destroy(true).await.unwrap_err());
    Fixture::assert_read_error(f.refresh().await.unwrap_err());
    assert_eq!(f.bytes(), before);
    let log = fs::read_to_string(&f.log).unwrap();
    assert!(log.contains("read test.resource.unclaimed"), "{log}");
    assert!(
        !log.lines().any(|line| line.ends_with("test.resource.old")),
        "{log}"
    );
}

#[tokio::test]
async fn removed_no_refresh_deferred_child_collision() {
    let f = Fixture::new();
    let mut saved: serde_json::Value = serde_json::from_slice(&f.bytes()).unwrap();
    saved["resources"][0]["identity"] = serde_json::json!("children[0]");
    fs::write(
        f.dir.path().join("carina.state.json"),
        serde_json::to_vec_pretty(&saved).unwrap(),
    )
    .unwrap();
    Fixture::assert_read_error(f.plan(true, None).await.unwrap_err());
    f.clear_log();
    fs::write(
        f.dir.path().join("main.crn"),
        r#"
        let parent = mock.test.resource {
            name = "parent"
            rules = [{ action = "child" }]
        }
        let children = for rule in parent.rules {
            mock.test.resource { name = rule.action }
        }
        removed { from = mock.test.resource 'children[0]' }
    "#,
    )
    .unwrap();
    let before = f.bytes();
    let error = f.plan(true, None).await.unwrap_err();
    assert!(
        error.to_string().contains("removed block from")
            && error.to_string().contains("collides with desired resource"),
        "{error}"
    );
    assert_eq!(f.bytes(), before);
    let log = fs::read_to_string(&f.log).unwrap_or_default();
    assert!(!log.lines().any(|line| line.starts_with("read ")), "{log}");
}

#[tokio::test]
async fn removed_no_refresh_destroy_persists_before_delete_failure() {
    let f = Fixture::new();
    let mut saved: serde_json::Value = serde_json::from_slice(&f.bytes()).unwrap();
    let mut unclaimed = saved["resources"][0].clone();
    unclaimed["identity"] = serde_json::json!("unclaimed");
    saved["resources"].as_array_mut().unwrap().push(unclaimed);
    fs::write(
        f.dir.path().join("carina.state.json"),
        serde_json::to_vec_pretty(&saved).unwrap(),
    )
    .unwrap();
    f.config(true);
    let error = f.destroy(false).await.unwrap_err();
    let crate::error::AppError::ExecutionFailed(report) = error else {
        panic!("expected a provider delete failure: {error}");
    };
    assert_eq!(report.failures().len(), 1);
    assert!(
        report.failures()[0]
            .to_string()
            .contains("Failed to load mock state")
    );
    let state: serde_json::Value = serde_json::from_slice(&f.bytes()).unwrap();
    let rows = state["resources"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["identity"], "unclaimed");
    let log = fs::read_to_string(&f.log).unwrap_or_default();
    assert!(
        !log.lines().any(|line| line.ends_with("test.resource.old")),
        "{log}"
    );
}

#[tokio::test]
async fn removed_no_refresh_state_preserves_deposed_read_behavior() {
    let f = Fixture::new();
    let mut saved: serde_json::Value = serde_json::from_slice(&f.bytes()).unwrap();
    let generation = carina_state::DeposedInstance::from_persisted_json(
        serde_json::from_value(serde_json::json!("historical")).unwrap(),
        "historical-id".into(),
        None,
        Default::default(),
        Default::default(),
    );
    saved["resources"][0]["deposed"] = serde_json::json!([generation]);
    fs::write(
        f.dir.path().join("carina.state.json"),
        serde_json::to_vec_pretty(&saved).unwrap(),
    )
    .unwrap();
    Fixture::assert_read_error(f.refresh().await.unwrap_err());
    f.clear_log();
    f.config(true);
    f.refresh().await.unwrap();
    let state: serde_json::Value = serde_json::from_slice(&f.bytes()).unwrap();
    assert_eq!(state["resources"].as_array().unwrap().len(), 1);
    assert_eq!(
        state["resources"][0]["deposed"].as_array().unwrap().len(),
        1
    );
    let validated = carina_state::check_and_migrate_bytes(&f.bytes()).unwrap();
    assert!(!validated.state().resources()[0].has_current_instance());
    let log = fs::read_to_string(&f.log).unwrap();
    assert_eq!(
        log.lines().filter(|line| line.starts_with("read ")).count(),
        1,
        "only the deposed generation is read: {log}"
    );
}
