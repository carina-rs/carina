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
use carina_core::provider::{AppliedIdentifier, NoopNormalizer, Provider};
use carina_core::resource::{
    ConcreteValue, Current, ResolvedResource, ResolvedResourceId, ResourceId, State, Value,
};
use carina_state::{ResourceState, StateFile};

#[path = "../../carina-provider-mock/tests/support/provider_text_secret.rs"]
mod fixture;
use fixture::{EchoFactory, EchoMock, Failure, MASK, PUBLIC, SECRET};

const CHILD: &str = "CARINA_4305_PROVIDER_TEXT_CHILD";

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
    provider: &dyn Provider,
    plan: &Plan,
    current: HashMap<ResourceId, Current>,
) -> ExecutionResult {
    let schemas = fixture::schemas();
    let input = LiveExecutionInput {
        plan,
        provider_check_inputs: ProviderCheckInputs::Authored,
        compositions: &[],
        bindings: ResolvedBindings::default(),
        managed_current: current,
        data_source_values: ExecutableDataSourceValues::empty(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
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
        let provider = EchoMock::new(failure);
        let desired = resource(plaintext, secret);
        let mut plan = Plan::new();
        let mut current = HashMap::new();
        match failure {
            Failure::Delete => {
                let mut create = Plan::new();
                create.add(Effect::Create(desired.clone()));
                let created = execute(&provider, &create, HashMap::new()).await;
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
        let result = execute(&provider, &plan, current).await;
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
        format!("provider mock {{ password = secret(\"{SECRET}\") }}\n"),
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
    if failure == Failure::Refresh {
        let mut state = StateFile::new();
        let mut row = ResourceState::try_applied("test.resource", "probe", "mock", "mock-id")
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
