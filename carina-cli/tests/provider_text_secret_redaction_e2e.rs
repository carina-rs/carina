//! carina#4305: provider plaintext must be removed at real CLI output boundaries.
//! The child process captures OS stderr, including the private progress sink.

use std::collections::HashMap;
use std::path::Path;
use std::process::{Command, Output};

use carina_cli::test_support::{
    TestCommandRuntime, execute_live_plan_with_cli_observer_for_test,
    run_refreshing_plan_with_factories,
};
use carina_cli::wiring::LoadedProviderFactory;
use carina_core::binding_index::ResolvedBindings;
use carina_core::data_source_value::ExecutableDataSourceValues;
use carina_core::effect::{Effect, EffectGeneration};
use carina_core::executor::{
    ExecutionCheckpointSink, ExecutionOutcome, ExecutionResult, LiveExecutionInput,
    ProviderCheckInputs,
};
use carina_core::parser::{ProviderContext, parse_file_and_resolve};
use carina_core::plan::Plan;
use carina_core::provider::{AppliedIdentifier, HostProvider, NoopNormalizer};
use carina_core::resource::{
    ConcreteValue, Current, ResolvedResource, ResolvedResourceId, ResourceId, State, Value,
};
use carina_state::{ResourceState, StateFile};

#[path = "../../carina-provider-mock/tests/support/provider_text_secret.rs"]
mod fixture;
use fixture::{EchoFactory, EchoMock, Failure, MASK, PUBLIC, SECRET};

const CHILD: &str = "CARINA_4305_PROVIDER_TEXT_CHILD";

#[path = "provider_text_secret_redaction_e2e/round5.rs"]
mod round5;

#[path = "provider_text_secret_redaction_e2e/update_partial.rs"]
mod update_partial;

#[test]
fn review_round7_bound_error_relay_through_state_only_boundary_masks_cli_display() {
    use carina_core::provider::ProviderError;
    use fixture::text_fixtures::BindProviderText;
    const NAME: &str =
        "review_round7_bound_error_relay_through_state_only_boundary_masks_cli_display";
    if let Some(stderr) = in_child(NAME, || {
        let bound = ProviderError::api_error(format!("public context {SECRET}"))
            .with_secrets_for_test(&fixture::text_fixtures::context(&[SECRET]));
        let relayed: ProviderError = bound.into();
        let rebound = fixture::text_fixtures::rebind_state_only(relayed);
        eprintln!("{}", carina_cli::error::AppError::from(rebound));
    }) {
        assert_redacted(&stderr, SECRET);
        assert!(stderr.contains("public context"));
    }
}

fn in_child(name: &str, exercise: impl FnOnce()) -> Option<String> {
    in_child_output(name, exercise).map(|output| String::from_utf8(output.stderr).unwrap())
}

fn in_child_output(name: &str, exercise: impl FnOnce()) -> Option<Output> {
    if std::env::var(CHILD).as_deref() == Ok(name) {
        exercise();
        return None;
    }
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture"])
        .env(CHILD, name)
        .env("NO_COLOR", "1")
        .env_remove("CLICOLOR_FORCE")
        .output()
        .expect("capture the real CLI progress/error stderr in a child process");
    assert!(
        output.status.success(),
        "fixture child failed before its redaction assertion:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Some(output)
}

fn assert_redacted(text: &str, plaintext: &str) {
    // Check the whole sink before requiring a mask: the current failure must
    // identify an actual disclosure rather than merely a missing placeholder.
    assert!(
        !text.contains(plaintext),
        "provider secret plaintext leaked to CLI output:\n{text}"
    );
    assert!(
        text.contains(MASK),
        "missing {MASK} in provider diagnostic:\n{text}"
    );
}

fn resource(plaintext: &str, secret: bool) -> ResolvedResource {
    let literal = serde_json::to_string(plaintext).unwrap();
    let password = if secret {
        format!("secret({literal})")
    } else {
        literal
    };
    let context = ProviderContext::new(carina_core::EnvironmentSnapshot::empty());
    let mut parsed = parse_file_and_resolve(
        &format!("provider mock {{}}\nlet probe = mock.test.resource {{ name = \"probe\" password = {password} }}\n"),
        Path::new("main.crn"), &context,
    ).unwrap().into_expanded(&fixture::schemas(), &context).unwrap();
    ResolvedResource::new(
        parsed
            .resources
            .remove(0)
            .into_resolved_dependency_phase_for_test(),
    )
}

async fn execute(
    provider: &dyn HostProvider,
    plan: &Plan,
    current: HashMap<ResourceId, Current>,
) -> ExecutionResult {
    let schemas = fixture::schemas();
    let checked = carina_core::input_rules::finalize_plan_for_test(plan.clone())
        .check_inputs(&schemas, &Default::default())
        .unwrap();
    let input = LiveExecutionInput {
        plan: &checked,
        provider_check_inputs: ProviderCheckInputs::Authored,
        compositions: &[],
        bindings: ResolvedBindings::default(),
        managed_current: current,
        data_source_values: ExecutableDataSourceValues::empty(),
        normalizer: &NoopNormalizer,
        provider_configs: &carina_core::test_utils::resolved_provider_configs(&[]),
        provider_context: ProviderContext::new(carina_core::EnvironmentSnapshot::empty()),
        factories: &[],
        schemas: &schemas,
        parallelism: carina_cli::DEFAULT_PARALLELISM,
        precompleted_success_count: 0,
        checkpoint_sink: ExecutionCheckpointSink::new(|_| {}),
    };
    match execute_live_plan_with_cli_observer_for_test(provider, input, Path::new(".")).await {
        ExecutionOutcome::Completed(result) => result,
        ExecutionOutcome::Cancelled(_) => panic!("fixture execution was cancelled"),
    }
}

fn crud(failure: Failure, plaintext: &str, secret: bool) {
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        // The host instance outlives both executions, as it does in the
        // runtime router. Delete/read retain the actual earlier call's set.
        let desired = resource(plaintext, secret);
        let host_instance = fixture::text_fixtures::initialized_for_desired(
            Box::new(EchoMock::new(failure)),
            std::slice::from_ref(desired.as_resource()),
        );
        let provider = host_instance.provider();
        let mut plan = Plan::new();
        let mut current = HashMap::new();
        match failure {
            Failure::Delete => {
                let mut create = Plan::new();
                create.add(Effect::Create(desired.clone()));
                let created = execute(provider, &create, HashMap::new()).await;
                assert_eq!(
                    created.success_count, 1,
                    "seed create must reach the native mock"
                );
                current = created.managed_current;
                plan.add(Effect::Delete {
                    id: ResolvedResourceId::new(desired.id.clone()),
                    identifier: AppliedIdentifier::new("mock-id").unwrap(),
                    generation: EffectGeneration::Current,
                    directives: desired.directives.clone(),
                    binding: desired.binding.clone(),
                    dependencies: Default::default(),
                    explicit_dependencies: Default::default(),
                    blocked_by_updates: Default::default(),
                });
            }
            Failure::Update | Failure::RecoveryRefresh => {
                let from = State::<carina_core::resource::NoPlaceholder>::existing(
                    desired.id.clone(),
                    HashMap::from([
                        (
                            "name".into(),
                            Value::Concrete(ConcreteValue::String("probe".into())),
                        ),
                        (
                            "password".into(),
                            Value::Concrete(ConcreteValue::String("old".into())),
                        ),
                    ]),
                )
                .with_identifier("mock-id")
                .to_snapshot();
                current.insert(desired.id.clone(), Current::from_snapshot(from.clone()));
                plan.add(Effect::Update {
                    from: Box::new(from),
                    identifier: AppliedIdentifier::new("mock-id").unwrap(),
                    to: desired,
                    changed_attributes: vec!["password".into()],
                });
            }
            _ => plan.add(Effect::Create(desired)),
        }
        let result = execute(provider, &plan, current).await;
        assert_eq!(
            result.failure_count(),
            1,
            "fixture must produce a provider operation failure"
        );
    });
}

fn assert_crud(name: &str, failure: Failure, plaintext: &str) {
    if let Some(stderr) = in_child(name, || crud(failure, plaintext, true)) {
        assert!(
            stderr.contains("mock") && stderr.contains("rejected password"),
            "provider failure was not rendered:\n{stderr}"
        );
        assert!(
            stderr.contains(PUBLIC),
            "non-secret diagnostic context was lost:\n{stderr}"
        );
        assert_redacted(&stderr, plaintext);
    }
}

#[test]
fn create_message_secret_is_redacted_from_cli_stderr() {
    assert_crud(
        "create_message_secret_is_redacted_from_cli_stderr",
        Failure::CreateMessage,
        SECRET,
    );
}

#[test]
fn create_cause_only_secret_is_redacted_from_cli_stderr() {
    assert_crud(
        "create_cause_only_secret_is_redacted_from_cli_stderr",
        Failure::CreateCause,
        SECRET,
    );
}

#[test]
fn update_failure_secret_is_redacted_from_cli_stderr() {
    assert_crud(
        "update_failure_secret_is_redacted_from_cli_stderr",
        Failure::Update,
        SECRET,
    );
}

#[test]
fn delete_failure_secret_is_redacted_from_cli_stderr() {
    assert_crud(
        "delete_failure_secret_is_redacted_from_cli_stderr",
        Failure::Delete,
        SECRET,
    );
}

#[test]
fn recovery_refresh_warning_secret_is_redacted_from_cli_output() {
    // Recovery warnings use the observer's ordinary output route (stdout in
    // plain mode), while the primary operation failure goes to stderr.
    if let Some(output) = in_child_output(
        "recovery_refresh_warning_secret_is_redacted_from_cli_output",
        || crud(Failure::RecoveryRefresh, SECRET, true),
    ) {
        let stderr = format!(
            "{}{}",
            String::from_utf8(output.stdout).unwrap(),
            String::from_utf8(output.stderr).unwrap()
        );
        assert!(
            stderr.contains("mock update rejected request"),
            "primary update failure was not rendered:\n{stderr}"
        );
        assert!(
            stderr.contains("! Refresh") && stderr.contains("mock recovery read rejected password"),
            "recovery refresh warning was not rendered:\n{stderr}"
        );
        assert!(
            stderr.contains(PUBLIC),
            "non-secret warning context was lost:\n{stderr}"
        );
        assert_redacted(&stderr, SECRET);
    }
}

#[test]
fn short_secret_is_redacted_from_cli_stderr() {
    assert_crud(
        "short_secret_is_redacted_from_cli_stderr",
        Failure::CreateMessage,
        "ab",
    );
}

#[test]
fn json_escaped_secret_is_redacted_from_cli_stderr() {
    let plaintext = "a\"b\\c";
    if let Some(stderr) = in_child("json_escaped_secret_is_redacted_from_cli_stderr", || {
        crud(Failure::JsonEscaped, plaintext, true)
    }) {
        assert!(
            stderr.contains(PUBLIC),
            "provider failure was not rendered:\n{stderr}"
        );
        let encoded = serde_json::to_string(plaintext).unwrap();
        assert_redacted(&stderr, &encoded[1..encoded.len() - 1]);
        assert!(
            !stderr.contains(plaintext),
            "raw secret also leaked:\n{stderr}"
        );
    }
}

#[test]
fn repeated_secret_is_redacted_every_time_from_cli_stderr() {
    if let Some(stderr) = in_child(
        "repeated_secret_is_redacted_every_time_from_cli_stderr",
        || crud(Failure::Repeated, SECRET, true),
    ) {
        assert!(
            stderr.contains(PUBLIC),
            "provider failure was not rendered:\n{stderr}"
        );
        assert_redacted(&stderr, SECRET);
        assert!(
            stderr.contains(&format!("{MASK} then {MASK}")),
            "both occurrences must be masked:\n{stderr}"
        );
    }
}

#[test]
fn non_secret_provider_text_is_shown_verbatim() {
    if let Some(stderr) = in_child("non_secret_provider_text_is_shown_verbatim", || {
        crud(Failure::CreateMessage, PUBLIC, false)
    }) {
        assert!(
            stderr.contains(&format!(
                "mock create rejected password {PUBLIC}; public={PUBLIC}"
            )),
            "non-secret provider text was changed:\n{stderr}"
        );
        assert!(
            !stderr.contains(MASK),
            "non-secret input was masked:\n{stderr}"
        );
    }
}

fn plan_failure(failure: Failure) {
    let directory = tempfile::tempdir().unwrap();
    let state_path = directory.path().join("carina.state.json");
    std::fs::write(
        directory.path().join("backend.crn"),
        format!(
            "backend local {{ path = {} }}\n",
            serde_json::to_string(&state_path).unwrap(),
        ),
    )
    .unwrap();
    std::fs::write(
        directory.path().join("carina-backend.lock"),
        serde_json::to_string_pretty(
            &serde_json::json!({ "backend_type": "local", "attributes": { "path": state_path } }),
        )
        .unwrap(),
    )
    .unwrap();
    std::fs::write(
        directory.path().join("providers.crn"),
        if matches!(failure, Failure::RefreshDesired | Failure::MovedRefresh) {
            "provider mock {}\n".to_owned()
        } else {
            format!("provider mock {{ password = secret(\"{SECRET}\") }}\n")
        },
    )
    .unwrap();
    let source = if failure == Failure::DataSource {
        format!("let probe = read mock.test.lookup {{ password = secret(\"{SECRET}\") }}\n")
    } else {
        format!(
            "let probe = mock.test.resource {{ name = \"probe\" password = secret(\"{SECRET}\") }}\n"
        )
    };
    std::fs::write(directory.path().join("main.crn"), source).unwrap();
    if matches!(
        failure,
        Failure::Refresh | Failure::RefreshDesired | Failure::MovedRefresh
    ) {
        let mut state = StateFile::new();
        let mut row = ResourceState::try_applied(
            "test.resource",
            if failure == Failure::MovedRefresh {
                "old-probe"
            } else {
                "probe"
            },
            "mock",
            "mock-id",
        )
        .unwrap()
        .with_attribute("name", serde_json::json!("probe"));
        row.binding = Some(carina_core::resource::absolute_binding_address_for_test(
            "probe",
        ));
        state.upsert_resource(row).unwrap();
        std::fs::write(
            directory.path().join("carina.state.json"),
            serde_json::to_string_pretty(&state).unwrap(),
        )
        .unwrap();
    }
    let runtime = TestCommandRuntime::new();
    let error = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(run_refreshing_plan_with_factories(
            &runtime,
            directory.path(),
            LoadedProviderFactory::for_test(vec![Box::new(EchoFactory(failure))]),
        ))
        .expect_err("plan must fail at the provider read boundary");
    assert!(
        matches!(error, carina_cli::error::AppError::Provider(_)),
        "expected provider read failure, got {error:?}"
    );
    // Same terminal error-chain formatter used by main's CLI error handler.
    eprintln!(
        "Error: {}",
        carina_cli::error::format_error_with_source_chain(&error)
    );
}

#[test]
fn refresh_plan_read_secret_is_redacted_from_cli_stderr() {
    if let Some(stderr) = in_child(
        "refresh_plan_read_secret_is_redacted_from_cli_stderr",
        || plan_failure(Failure::Refresh),
    ) {
        assert!(
            stderr.contains("mock refresh read rejected password") && stderr.contains(PUBLIC),
            "refresh failure was not rendered:\n{stderr}"
        );
        assert_redacted(&stderr, SECRET);
    }
}

#[test]
fn data_source_read_secret_is_redacted_from_cli_stderr() {
    if let Some(stderr) = in_child(
        "data_source_read_secret_is_redacted_from_cli_stderr",
        || plan_failure(Failure::DataSource),
    ) {
        assert!(
            stderr.contains("mock data source read rejected password") && stderr.contains(PUBLIC),
            "data source failure was not rendered:\n{stderr}"
        );
        assert_redacted(&stderr, SECRET);
    }
}

#[test]
fn provider_config_cause_secret_is_redacted_from_cli_validate() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(
        directory.path().join("providers.crn"),
        format!("provider mock {{ password = secret(\"{SECRET}\") }}\n"),
    )
    .unwrap();
    std::fs::write(
        directory.path().join("main.crn"),
        "mock.test.resource { name = \"probe\" }\n",
    )
    .unwrap();
    let errors = carina_cli::commands::validate::validate_with_factories(
        &carina_core::paths::ResolvedPath::new(directory.path()).unwrap(),
        LoadedProviderFactory::for_test(vec![Box::new(EchoFactory(Failure::ConfigCause))]),
    );
    let text = errors.join("\n");
    assert!(
        !errors.is_empty() && text.contains("failed validation"),
        "config validation was not reached:\n{text}"
    );
    assert!(
        text.contains(PUBLIC),
        "non-secret cause context was lost:\n{text}"
    );
    assert_redacted(&text, SECRET);
}

#[test]
fn review_a1_host_error_relay_is_redacted_by_real_cli_observer() {
    assert_crud(
        "review_a1_host_error_relay_is_redacted_by_real_cli_observer",
        Failure::HostRelay,
        SECRET,
    );
}

#[test]
fn review_a2_generic_host_cause_is_redacted_by_real_cli_observer() {
    assert_crud(
        "review_a2_generic_host_cause_is_redacted_by_real_cli_observer",
        Failure::HostCause,
        SECRET,
    );
}

#[test]
fn review_a7_refresh_uses_desired_secrets_without_config_or_prior_create() {
    if let Some(stderr) = in_child(
        "review_a7_refresh_uses_desired_secrets_without_config_or_prior_create",
        || plan_failure(Failure::RefreshDesired),
    ) {
        assert!(stderr.contains("mock refresh read rejected password"));
        assert_redacted(&stderr, SECRET);
    }
}

#[test]
fn review_b1_scalar_secrets_preserve_http_status_through_executor() {
    if let Some(stderr) = in_child(
        "review_b1_scalar_secrets_preserve_http_status_through_executor",
        || {
            tokio::runtime::Runtime::new().unwrap().block_on(async {
                let mut raw = resource(SECRET, true).as_resource().clone();
                raw.attributes.insert(
                    "numeric".into(),
                    Value::Deferred(carina_core::resource::DeferredValue::Secret(
                        Box::new(Value::Concrete(ConcreteValue::Int(3))).into(),
                    )),
                );
                raw.attributes.insert(
                    "boolean".into(),
                    Value::Deferred(carina_core::resource::DeferredValue::Secret(
                        Box::new(Value::Concrete(ConcreteValue::Bool(true))).into(),
                    )),
                );
                let instance = fixture::text_fixtures::initialized_for_desired(
                    Box::new(EchoMock::new(Failure::ScalarStatus)),
                    std::slice::from_ref(&raw),
                );
                let mut plan = Plan::new();
                plan.add(Effect::Create(ResolvedResource::new(raw)));
                let result = execute(instance.provider(), &plan, HashMap::new()).await;
                assert_eq!(result.failure_count(), 1);
            });
        },
    ) {
        assert!(
            stderr.contains("status: 403"),
            "HTTP status was damaged: {stderr}"
        );
        assert!(
            stderr.contains("password (secret) (secret)"),
            "scalar text was not redacted: {stderr}"
        );
    }
}

#[test]
fn review_a5_partial_missing_key_is_redacted_before_state_writeback() {
    use carina_state::backends::local_backend_for_test;
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let desired = resource(SECRET, true);
        let instance = fixture::text_fixtures::initialized_for_desired(
            Box::new(EchoMock::new(Failure::PartialMissingKey)),
            std::slice::from_ref(desired.as_resource()),
        );
        let id = desired.id.clone();
        let mut plan = Plan::new();
        plan.add(Effect::Create(desired));
        let result = execute(instance.provider(), &plan, HashMap::new()).await;
        let carina_core::executor::Applied::Complete {
            state: snapshot, ..
        } = result.applied_states.get(&id).unwrap()
        else {
            panic!("created state must be complete")
        };
        let row =
            ResourceState::from_provider_state("test.resource", "probe", "mock", snapshot).unwrap();
        let mut state = StateFile::new();
        state.upsert_resource(row).unwrap();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("carina.state.json");
        local_backend_for_test(path.clone())
            .write_state(&state)
            .await
            .unwrap();
        let text = std::fs::read_to_string(path).unwrap();
        assert!(
            !text.contains(SECRET),
            "provider key leaked through state writeback: {text}"
        );
        assert!(text.contains(MASK));
    });
}

#[test]
fn round4_destroy_refresh_uses_desired_secret_without_prior_call() {
    if let Some(stderr) = in_child(
        "round4_destroy_refresh_uses_desired_secret_without_prior_call",
        || {
            let directory = tempfile::tempdir().unwrap();
            std::fs::write(directory.path().join("main.crn"), "provider mock {}\nlet probe = mock.test.resource { name = \"probe\" password = secret(\"hunter2\") }\n").unwrap();
            let mut state = StateFile::new();
            state
                .upsert_resource(
                    ResourceState::try_applied("test.resource", "probe", "mock", "mock-id")
                        .unwrap(),
                )
                .unwrap();
            std::fs::write(
                directory.path().join("carina.state.json"),
                serde_json::to_string(&state).unwrap(),
            )
            .unwrap();
            let error = tokio::runtime::Runtime::new()
                .unwrap()
                .block_on(
                    carina_cli::commands::destroy::run_destroy_with_factories_for_test(
                        &TestCommandRuntime::new(),
                        directory.path(),
                        LoadedProviderFactory::for_test(vec![Box::new(EchoFactory(
                            Failure::DestroyRefresh,
                        ))]),
                    ),
                )
                .unwrap_err();
            eprintln!(
                "Error: {}",
                carina_cli::error::format_error_with_source_chain(&error)
            );
        },
    ) {
        assert!(
            stderr.contains("refresh read rejected password"),
            "{stderr}"
        );
        assert_redacted(&stderr, "hunter2");
    }
}

#[test]
fn round4_moved_resource_refresh_uses_desired_secret_under_loaded_id() {
    if let Some(stderr) = in_child(
        "round4_moved_resource_refresh_uses_desired_secret_under_loaded_id",
        || {
            plan_failure(Failure::MovedRefresh);
        },
    ) {
        assert!(
            stderr.contains("refresh read rejected password"),
            "{stderr}"
        );
        assert_redacted(&stderr, SECRET);
    }
}

#[test]
fn round4_unknown_missing_attribute_is_a_typed_error() {
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let desired = resource(SECRET, true);
        let provider = fixture::text_fixtures::initialized_for_desired(
            Box::new(EchoMock::new(Failure::UnknownMissing)),
            std::slice::from_ref(desired.as_resource()),
        );
        let mut plan = Plan::new();
        plan.add(Effect::Create(desired));
        let result = execute(provider.provider(), &plan, HashMap::new()).await;
        assert_eq!(
            result.failure_count(),
            1,
            "an unknown schema name must be rejected"
        );
        let carina_core::executor::EffectFailureKind::Provider { error, .. } =
            result.failures()[0].kind()
        else {
            panic!("provider failure")
        };
        let failure = error
            .as_error()
            .downcast_ref::<carina_core::provider::HostError>()
            .unwrap();
        assert_eq!(
            failure.kind(),
            carina_core::provider::ProviderErrorKind::InvalidInput
        );
        assert!(
            failure
                .detail()
                .cause()
                .unwrap()
                .is::<carina_core::provider::UnknownMissingAttribute>()
        );
        assert_eq!(
            result.applied_states.len(),
            1,
            "the invalid diagnostic must not lose the created identifier"
        );
    });
}

#[test]
fn round4_short_secrets_keep_missing_attribute_and_unknown_on_next_plan() {
    if let Some(output) = in_child_output(
        "round4_short_secrets_keep_missing_attribute_and_unknown_on_next_plan",
        || {
            tokio::runtime::Runtime::new().unwrap().block_on(async {
            let mut raw = resource(SECRET, true).as_resource().clone();
            raw.attributes.insert("numeric".into(), Value::Deferred(carina_core::resource::DeferredValue::Secret(Box::new(Value::Concrete(ConcreteValue::Int(1))).into())));
            raw.attributes.insert("boolean".into(), Value::Deferred(carina_core::resource::DeferredValue::Secret(Box::new(Value::Concrete(ConcreteValue::Bool(true))).into())));
            raw.attributes.insert("subnet_1_id".into(), Value::Concrete(ConcreteValue::String("wanted".into())));
            let desired = ResolvedResource::new(raw);
            let provider = fixture::text_fixtures::initialized_for_desired(Box::new(EchoMock::new(Failure::ShortMissing)), std::slice::from_ref(desired.as_resource()));
            let mut plan = Plan::new(); plan.add(Effect::Create(desired.clone()));
            let result = execute(provider.provider(), &plan, HashMap::new()).await;
            assert_eq!(result.partial_count, 1);
            let carina_core::executor::Applied::Complete { state: snapshot, .. } = &result.applied_states[&desired.id] else { panic!("partial create has state") };
            assert_eq!(snapshot.partial_read.as_ref().unwrap().missing_attributes().iter().next().unwrap(), "subnet_1_id");
            let mut state = StateFile::new();
            state.upsert_resource(ResourceState::from_provider_state("test.resource", "probe", "mock", snapshot).unwrap()).unwrap();
            let directory = tempfile::tempdir().unwrap();
            let state_path = directory.path().join("carina.state.json");
            std::fs::write(&state_path, serde_json::to_string(&state).unwrap()).unwrap();
            std::fs::write(directory.path().join("carina-backend.lock"), serde_json::to_string(&serde_json::json!({ "backend_type": "local", "attributes": { "path": state_path } })).unwrap()).unwrap();
            std::fs::write(directory.path().join("main.crn"), format!("backend local {{ path = {} }}\nprovider mock {{}}\nlet probe = mock.test.resource {{ name = \"probe\" password = secret(\"{SECRET}\") numeric = secret(1) boolean = secret(true) subnet_1_id = \"wanted\" }}\n", serde_json::to_string(&state_path).unwrap())).unwrap();
            carina_cli::test_support::run_detailed_plan_with_factories(&TestCommandRuntime::new(), directory.path(), LoadedProviderFactory::for_test(vec![Box::new(EchoFactory(Failure::ShortMissing))])).await.unwrap();
        });
        },
    ) {
        let text = format!(
            "{}{}",
            String::from_utf8(output.stdout).unwrap(),
            String::from_utf8(output.stderr).unwrap()
        );
        assert!(text.contains("subnet_1_id"), "{text}");
        assert!(text.contains("unknown until next refresh"), "{text}");
    }
}

#[test]
fn round4_host_status_preserves_digits_while_provider_digits_are_masked() {
    if let Some(stderr) = in_child(
        "round4_host_status_preserves_digits_while_provider_digits_are_masked",
        || {
            tokio::runtime::Runtime::new().unwrap().block_on(async {
                let desired = resource("4", true);
                let instance = fixture::text_fixtures::initialized_for_desired(
                    Box::new(EchoMock::new(Failure::HostStatus)),
                    std::slice::from_ref(desired.as_resource()),
                );
                let mut plan = Plan::new();
                plan.add(Effect::Create(desired));
                let result = execute(instance.provider(), &plan, HashMap::new()).await;
                assert_eq!(result.failure_count(), 1);
                let carina_core::executor::EffectFailureKind::Provider { error, .. } =
                    result.failures()[0].kind()
                else {
                    panic!("provider failure")
                };
                let provider = error
                    .as_error()
                    .downcast_ref::<carina_core::provider::HostError>()
                    .unwrap()
                    .clone();
                let host = carina_core::provider::HostError::internal_caused_by(
                    "host status: 404",
                    provider,
                );
                eprintln!(
                    "Error: {}",
                    carina_cli::error::format_error_with_source_chain(
                        &carina_cli::error::AppError::Provider(host)
                    )
                );
            });
        },
    ) {
        assert!(stderr.contains("status: 404"), "{stderr}");
        assert!(stderr.contains("host status: 404"), "{stderr}");
        assert!(
            stderr.contains("provider digits (secret)0(secret)"),
            "{stderr}"
        );
    }
}

#[test]
fn round4_transient_classification_survives_boundary_and_executor() {
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let desired = resource(SECRET, true);
        let instance = fixture::text_fixtures::initialized_for_desired(
            Box::new(EchoMock::new(Failure::Transient)),
            std::slice::from_ref(desired.as_resource()),
        );
        let mut plan = Plan::new();
        plan.add(Effect::Create(desired));
        let result = execute(instance.provider(), &plan, HashMap::new()).await;
        assert_eq!(result.failure_count(), 1);
        let carina_core::executor::EffectFailureKind::Provider { error, .. } =
            result.failures()[0].kind()
        else {
            panic!("provider failure")
        };
        let failure = error
            .as_error()
            .downcast_ref::<carina_core::provider::HostError>()
            .unwrap();
        assert_eq!(
            failure.retryability(),
            carina_core::provider_text::Retryability::Transient
        );
        assert!(failure.is_transient());
        assert!(!failure.to_string().contains(SECRET));
    });
}

#[test]
fn round4_state_only_destroy_uses_config_and_desired_set() {
    use carina_core::provider::*;
    use carina_core::schema::{
        ValidatedConfigSchema, ValidatedProviderSchemaWitness, ValidatedResourceSchema,
    };
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    struct Factory(Arc<AtomicUsize>);
    impl ProviderFactory for Factory {
        fn name(&self) -> &str {
            "mock"
        }
        fn display_name(&self) -> &str {
            "Mock"
        }
        fn provider_config_attribute_types(&self) -> ValidatedConfigSchema {
            EchoFactory(Failure::StateOnly).provider_config_attribute_types()
        }
        fn schemas(&self) -> Vec<ValidatedResourceSchema> {
            EchoFactory(Failure::StateOnly).schemas()
        }
        fn schema_witness(&self) -> ValidatedProviderSchemaWitness {
            EchoFactory(Failure::StateOnly).schema_witness()
        }
        fn validate_config(
            &self,
            _: &CanonicalProviderConfigAttributes,
            _: &ProviderValidationInputs,
        ) -> ProviderResult<()> {
            Ok(())
        }
        fn create_provider(
            &self,
            session: &ProviderSession,
            _: InitializeOutcomeMint,
        ) -> BoxFuture<'_, ProviderResult<ProviderInitialization>> {
            // State holds only secret hashes, so this resource's plaintext was
            // never sent in this run and cannot appear in provider text.
            let mut writer = carina_core::provider_text::RedactingLineWriter::new(
                session.redaction_set(),
                std::io::stderr(),
            );
            writer
                .push_str("inventory cfg-secret desired-secret state-only-public\n")
                .unwrap();
            writer.finish().unwrap();
            let provider = EchoMock::new(Failure::StateOnly).with_delete_counter(self.0.clone());
            Box::pin(async move {
                Ok(InitializeOutcome::unavailable().with_provider(Box::new(provider)))
            })
        }
    }
    if let Some(stderr) = in_child(
        "round4_state_only_destroy_uses_config_and_desired_set",
        || {
            let directory = tempfile::tempdir().unwrap();
            std::fs::write(directory.path().join("main.crn"), "provider mock { password = secret(\"cfg-secret\") }\nlet probe = mock.test.resource { name = \"probe\" password = secret(\"desired-secret\") }\n").unwrap();
            let mut state = StateFile::new();
            state.upsert_resource(ResourceState::try_applied("test.resource", "orphan", "mock", "mock-id").unwrap().with_attribute("name", serde_json::json!("orphan")).with_attribute("password", serde_json::json!({carina_core::value::PERSISTED_SECRET_KEY: format!("argon2-legacy:73616c74:{}", "01".repeat(32))}))).unwrap();
            let state_path = directory.path().join("carina.state.json");
            std::fs::write(&state_path, serde_json::to_string(&state).unwrap()).unwrap();
            let deleted = Arc::new(AtomicUsize::new(0));
            tokio::runtime::Runtime::new()
                .unwrap()
                .block_on(
                    carina_cli::commands::destroy::run_destroy_with_factories_for_test(
                        &TestCommandRuntime::new(),
                        directory.path(),
                        LoadedProviderFactory::for_test(vec![Box::new(Factory(deleted.clone()))]),
                    ),
                )
                .unwrap();
            assert_eq!(deleted.load(Ordering::SeqCst), 1);
            assert!(
                serde_json::from_str::<serde_json::Value>(
                    &std::fs::read_to_string(state_path).unwrap()
                )
                .unwrap()["resources"]
                    .as_array()
                    .unwrap()
                    .is_empty()
            );
        },
    ) {
        assert!(
            stderr.contains("inventory (secret) (secret) state-only-public"),
            "{stderr}"
        );
        assert!(!stderr.contains("cfg-secret"));
        assert!(!stderr.contains("desired-secret"));
    }
}
