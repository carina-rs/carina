//! carina#4230: named primitive references defer anonymous sink constraints.
//!
//! The built-in mock catalog has no named output / anonymous refined input
//! pairs for these cases. Use the existing caller-supplied factory seam instead
//! of changing that production catalog. Like the #3825 plan-display fixture,
//! runtime tests skip resource type checking in the fixture planner so they
//! independently pin #3805. The validate tests cover the authored boundary,
//! and an additional test runs the full plan command with injected factories.

use std::collections::HashMap;
use std::fs;
use std::sync::{Arc, Mutex};

use carina_cli::fixture_plan::{FixturePlan, build_plan_from_fixture_path_using};
use carina_cli::test_support::{TestCommandRuntime, run_plan_with_factories};
use carina_cli::wiring::LoadedProviderFactory;
use carina_core::binding_index::ResolvedBindings;
use carina_core::data_source_value::ExecutableDataSourceValues;
use carina_core::executor::{
    Applied, EffectFailureKind, ExecutionCheckpointSink, ExecutionObserver, ExecutionOutcome,
    ExecutionResult, LiveExecutionInput, ProviderCheckInputs, ProviderPreparationError,
    execute_live_plan_for_test,
};
use carina_core::plan::PlanErrorKind;
use carina_core::provider::{
    AppliedIdentifier, BoxFuture, CreateAttempt, CreateFailure, CreateOutcome, CreateRequest,
    NoopNormalizer, Provider, ProviderFactory, ProviderReadyDataSource, ProviderResult,
    ReadOutcome, ReadRequest, ResumeCreateRequest, ResumeFailure, ResumeOutcome, Sent,
    UpdateOutcome, UpdateRequest,
};
use carina_core::resource::{ConcreteValue, Current, DeferredValue, ResourceId, State, Value};
use carina_core::schema::{
    AttributeSchema, AttributeType, ResourceSchema, StructField, TypeError, TypeIdentity,
    ValidatedResourceSchema,
};
use carina_state::{ResourceState, StateFile};
use tempfile::TempDir;

const PRODUCER: &str = "test.TypedProducer";
const CONSUMER: &str = "test.RefinedConsumer";
const PATTERN: &str = "^good-[a-z]+$";

struct TypedRefMockFactory;

impl ProviderFactory for TypedRefMockFactory {
    fn name(&self) -> &str {
        "mock"
    }

    fn display_name(&self) -> &str {
        "Typed reference constraint mock"
    }

    fn provider_config_attribute_types(&self) -> carina_core::schema::ValidatedConfigSchema {
        Default::default()
    }

    fn validate_config(
        &self,
        _: &carina_core::provider::CanonicalProviderConfigAttributes,
        _session: &carina_core::provider::ProviderValidationInputs,
    ) -> ProviderResult<()> {
        Ok(())
    }

    fn create_provider(
        &self,
        _: &carina_core::provider::ProviderSession,
        _: carina_core::provider::InitializeOutcomeMint,
    ) -> BoxFuture<'_, ProviderResult<carina_core::provider::ProviderInitialization>> {
        Box::pin(async {
            Ok(
                carina_core::provider::InitializeOutcome::unavailable().with_provider(Box::new(
                    OutputMock {
                        output: Value::Concrete(ConcreteValue::String("good-value".into())),
                        port: 80,
                        creates: Mutex::new(Vec::new()),
                    },
                )),
            )
        })
    }

    fn schema_witness(&self) -> carina_core::schema::ValidatedProviderSchemaWitness {
        carina_core::schema::ValidatedProviderSchemaWitness::new(
            self.schemas(),
            self.provider_config_attribute_types(),
            Default::default(),
            Vec::new(),
        )
        .expect("typed reference mock witness")
    }

    fn schemas(&self) -> Vec<ValidatedResourceSchema> {
        let named = AttributeType::refined_string(
            Some(TypeIdentity::new(Some("mock"), Vec::<String>::new(), "Arn")),
            None,
            None,
            None,
        );
        let refined = AttributeType::refined_string(
            None,
            Some(carina_core::schema::PatternSpec::new(
                PATTERN.to_string(),
                carina_core::schema::PatternMatch::Full,
            )),
            Some((Some(6), Some(12))),
            None,
        );
        [
            ResourceSchema::new(PRODUCER)
                .attribute(AttributeSchema::provider_populated("typed_out", named).build())
                .attribute(
                    AttributeSchema::provider_populated(
                        "typed_port",
                        AttributeType::refined_int(
                            Some(TypeIdentity::new(
                                Some("mock"),
                                Vec::<String>::new(),
                                "Port",
                            )),
                            None,
                        ),
                    )
                    .build(),
                ),
            ResourceSchema::new(CONSUMER)
                .attribute(AttributeSchema::optional("x", refined.clone()).build())
                .attribute(
                    AttributeSchema::optional(
                        "port",
                        AttributeType::refined_int(None, Some((Some(1), Some(1024)))),
                    )
                    .build(),
                )
                .attribute(
                    AttributeSchema::optional("xs", AttributeType::list(refined.clone())).build(),
                )
                .attribute(
                    AttributeSchema::optional(
                        "dimensions",
                        AttributeType::list(AttributeType::struct_(
                            "Dimension",
                            vec![
                                StructField::required("name", AttributeType::string()).build(),
                                StructField::required("value", refined).build(),
                            ],
                        )),
                    )
                    .with_block_name("dimensions")
                    .build(),
                ),
        ]
        .into_iter()
        .map(|schema| schema.validate().expect("typed reference mock schema"))
        .collect()
    }
}

fn factories() -> Vec<LoadedProviderFactory> {
    LoadedProviderFactory::for_test(vec![Box::new(TypedRefMockFactory)])
}

struct Scenario {
    project: TempDir,
}

impl Scenario {
    fn new(consumer_attributes: &str) -> Self {
        let project = tempfile::tempdir().expect("project directory");
        for (name, body) in [
            (
                "providers.crn",
                "backend local {}\nprovider mock {}\n".to_string(),
            ),
            (
                "producer.crn",
                "let producer = mock.test.TypedProducer {}\n".to_string(),
            ),
            (
                "consumer.crn",
                format!("let consumer = mock.test.RefinedConsumer {{\n{consumer_attributes}\n}}\n"),
            ),
        ] {
            fs::write(project.path().join(name), body).expect("write fixture file");
        }
        Self { project }
    }

    fn validate(&self) -> Vec<String> {
        carina_cli::commands::validate::validate_with_factories(
            &carina_core::paths::ResolvedPath::new(self.project.path()).unwrap(),
            factories(),
        )
    }

    fn seed_output(&self, value: &str) {
        self.seed_producer_output("typed_out", serde_json::json!(value));
    }

    fn seed_producer_output(&self, attribute: &str, value: serde_json::Value) {
        let mut producer = ResourceState::try_applied(PRODUCER, "producer", "mock", "producer-id")
            .unwrap()
            .with_attribute(attribute, value);
        producer.binding = Some(carina_core::resource::absolute_binding_address_for_test(
            "producer",
        ));
        let mut state = StateFile::new();
        state
            .upsert_resource(producer)
            .expect("seed producer state");
        fs::write(
            self.project.path().join("carina.state.json"),
            carina_core::utils::pretty_with_newline(&state).unwrap(),
        )
        .expect("write producer state");
    }

    fn plan(&self) -> FixturePlan {
        build_plan_from_fixture_path_using(&TestCommandRuntime::new(), self.project.path(), |_| {
            factories()
        })
        .expect("directory fixture must reach plan construction")
    }
}

#[test]
fn typed_ref_validate_accepts_anonymous_refined_scalar_sink() {
    let errors = Scenario::new("  x = producer.typed_out").validate();
    assert!(errors.is_empty(), "{}", errors.join("\n"));
}

#[test]
fn deferred_interpolation_checks_resolved_constraints_at_plan() {
    for through_let in [false, true] {
        let rhs = if through_let {
            "constructed"
        } else {
            "\"${producer.typed_out}\""
        };
        let scenario = Scenario::new(&format!("  x = {rhs}"));
        fs::write(
            scenario.project.path().join("aliases.crn"),
            "let constructed = \"${producer.typed_out}\"\n",
        )
        .unwrap();
        assert!(scenario.validate().is_empty());

        let providers = carina_core::provider::loaded_provider_snapshot_from_factories(
            Arc::new(vec![
                Box::new(TypedRefMockFactory) as Box<dyn ProviderFactory>
            ]),
            carina_core::schema::SchemaRegistry::new(),
            carina_core::EnvironmentSnapshot::empty(),
        );
        let engine = carina_lsp::diagnostics::DiagnosticEngine::new(
            Arc::new(providers),
            vec!["mock".into()],
        );
        let path = scenario.project.path().join("consumer.crn");
        let doc = carina_lsp::document::Document::new_with_path(
            fs::read_to_string(&path).unwrap(),
            path.clone(),
            Arc::new(carina_core::parser::ProviderContext::new(
                carina_core::EnvironmentSnapshot::empty(),
            )),
        );
        let diagnostics = engine.analyze(&doc, Some(&path));
        assert!(
            !diagnostics.iter().any(|diagnostic| diagnostic.severity
                == Some(tower_lsp::lsp_types::DiagnosticSeverity::ERROR)),
            "{diagnostics:#?}"
        );

        scenario.seed_output("bad-id");
        let fixture = scenario.plan();
        assert_eq!(
            fixture.plan.errors().len(),
            1,
            "{:?}",
            fixture.plan.errors()
        );
        assert!(
            matches!(&fixture.plan.errors()[0].kind, PlanErrorKind::ResolvedValueConstraint { message, .. } if message.as_str().contains("pattern"))
        );
        carina_state::BackendLock::local_default()
            .save(&carina_core::paths::ResolvedPath::new(scenario.project.path()).unwrap())
            .unwrap();
        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(run_plan_with_factories(
                &TestCommandRuntime::new(),
                scenario.project.path(),
                factories(),
            ));
        assert!(
            matches!(result, Err(carina_cli::error::AppError::Validation(message)) if message == "plan failed with 1 error(s)")
        );
    }
}

#[test]
fn typed_ref_int_validate_accepts_anonymous_ranged_sink() {
    let errors = Scenario::new("  port = producer.typed_port").validate();
    assert!(errors.is_empty(), "{}", errors.join("\n"));
}

#[test]
fn typed_ref_validate_accepts_anonymous_refined_list_sink() {
    let errors = Scenario::new("  xs = [producer.typed_out]").validate();
    assert!(errors.is_empty(), "{}", errors.join("\n"));
}

#[test]
fn typed_ref_list_struct_sink_validates_and_plan_rejects_known_invalid_output() {
    let scenario = Scenario::new(
        r#"
  dimensions {
    name = "VpcId"
    value = producer.typed_out
  }
"#,
    );
    let errors = scenario.validate();
    assert!(errors.is_empty(), "{}", errors.join("\n"));

    for (value, reason) in [("bad-id", "pattern"), ("good-overlongvalue", "length")] {
        scenario.seed_output(value);
        let fixture = scenario.plan();
        let errors = fixture.plan.errors();
        assert_eq!(errors.len(), 1, "{errors:#?}");
        let error = &errors[0];
        assert_eq!(error.resource_id.resource_type, CONSUMER);
        assert_eq!(error.resource_id.identity_str(), Some("consumer"));
        let PlanErrorKind::ResolvedValueConstraint {
            attributes,
            origins,
            message,
        } = &error.kind
        else {
            panic!("expected ResolvedValueConstraint, got {error:#?}");
        };
        assert_eq!(attributes, &["dimensions"]);
        assert_eq!(origins, &["producer.typed_out"]);
        assert!(
            message
                .as_str()
                .contains("List item at index 0: Struct field 'value':"),
            "{error:#?}"
        );
        assert!(message.as_str().contains(reason), "{error:#?}");
    }
}

#[test]
fn typed_ref_validate_rejects_violating_literals() {
    for (value, reason) in [("bad-id", "pattern"), ("good-overlongvalue", "length")] {
        for attribute in [format!("  x = '{value}'"), format!("  xs = ['{value}']")] {
            let errors = Scenario::new(&attribute).validate();
            let message = errors.join("\n");
            assert!(
                !errors.is_empty(),
                "invalid literal must fail validate: {attribute}"
            );
            assert!(message.contains(reason), "{message}");
            assert!(
                !message.contains("cannot assign"),
                "expected concrete constraint error: {message}"
            );
        }
    }
}

#[test]
fn typed_ref_plan_rejects_known_output_with_resolved_value_constraint() {
    for (value, reason) in [("bad-id", "pattern"), ("good-overlongvalue", "length")] {
        for (attributes, path) in [
            ("  x = producer.typed_out", "x"),
            ("  xs = [producer.typed_out]", "xs"),
        ] {
            let scenario = Scenario::new(attributes);
            scenario.seed_output(value);
            let fixture = scenario.plan();
            let errors = fixture.plan.errors();
            assert_eq!(errors.len(), 1, "{errors:#?}");
            let error = &errors[0];
            assert_eq!(error.resource_id.resource_type, CONSUMER);
            assert_eq!(error.resource_id.identity_str(), Some("consumer"));
            let PlanErrorKind::ResolvedValueConstraint {
                attributes,
                origins,
                message,
            } = &error.kind
            else {
                panic!("expected ResolvedValueConstraint, got {error:#?}");
            };
            assert_eq!(attributes, &[path]);
            assert_eq!(origins, &["producer.typed_out"]);
            assert!(message.as_str().contains(path), "{error:#?}");
            if path == "xs" {
                assert!(message.as_str().contains("index 0"), "{error:#?}");
            }
            assert!(message.as_str().contains(reason), "{error:#?}");
        }
    }
}

#[test]
fn typed_ref_int_plan_rejects_known_out_of_range_output() {
    for value in [0, 1025] {
        let scenario = Scenario::new("  port = producer.typed_port");
        scenario.seed_producer_output("typed_port", serde_json::json!(value));
        let fixture = scenario.plan();
        let errors = fixture.plan.errors();
        assert_eq!(errors.len(), 1, "{errors:#?}");
        let error = &errors[0];
        assert_eq!(error.resource_id.resource_type, CONSUMER);
        assert_eq!(error.resource_id.identity_str(), Some("consumer"));
        let PlanErrorKind::ResolvedValueConstraint {
            attributes,
            origins,
            message,
        } = &error.kind
        else {
            panic!("expected ResolvedValueConstraint, got {error:#?}");
        };
        assert_eq!(attributes, &["port"]);
        assert_eq!(origins, &["producer.typed_port"]);
        assert!(
            message.as_str().contains("outside allowed range"),
            "{error:#?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn typed_ref_plan_command_validates_references_and_rejects_known_invalid_outputs() {
    for (value, valid) in [
        ("bad-id", false),
        ("good-overlongvalue", false),
        ("good-value", true),
    ] {
        for attributes in ["  x = producer.typed_out", "  xs = [producer.typed_out]"] {
            let scenario = Scenario::new(attributes);
            scenario.seed_output(value);
            carina_state::BackendLock::local_default()
                .save(&carina_core::paths::ResolvedPath::new(scenario.project.path()).unwrap())
                .expect("initialize local backend lock");
            let runtime = TestCommandRuntime::new();
            let result =
                run_plan_with_factories(&runtime, scenario.project.path(), factories()).await;
            if valid {
                assert!(result.expect("valid output must reach the command's successful plan"));
            } else {
                // The sibling fixture-plan test pins the structured
                // ResolvedValueConstraint details. The command renders those
                // diagnostics and returns this summary only after planning.
                let error = result.expect_err("known invalid output must fail the plan command");
                assert!(
                    matches!(
                        &error,
                        carina_cli::error::AppError::Validation(message)
                            if message == "plan failed with 1 error(s)"
                    ),
                    "expected a plan constraint failure after reference type checking: {error:#?}",
                );
            }
        }
    }
}

/// A test-local in-process mock: records every create that crosses the sealed
/// provider-ready boundary and publishes a configurable producer output.
struct OutputMock {
    output: Value,
    port: i64,
    creates: Mutex<Vec<ResourceId>>,
}

impl Provider for OutputMock {
    fn name(&self) -> &str {
        "mock"
    }

    fn read(
        &self,
        _: &ResourceId,
        _: &AppliedIdentifier,
        _: ReadRequest,
    ) -> BoxFuture<'_, ProviderResult<ReadOutcome>> {
        Box::pin(async { unreachable!("this fixture only creates resources") })
    }

    fn read_data_source(
        &self,
        _: &ProviderReadyDataSource,
    ) -> BoxFuture<'_, ProviderResult<State>> {
        Box::pin(async { unreachable!("this fixture has no data sources") })
    }

    fn create(
        &self,
        id: &ResourceId,
        request: CreateRequest,
        attempt: CreateAttempt,
    ) -> BoxFuture<'_, Result<CreateOutcome, CreateFailure>> {
        self.creates.lock().unwrap().push(id.clone());
        let mut attributes = request.resource().as_resource().resolved_attributes();
        if id.resource_type == PRODUCER {
            attributes.insert("typed_out".to_string(), self.output.clone());
            attributes.insert(
                "typed_port".to_string(),
                Value::Concrete(ConcreteValue::Int(self.port)),
            );
        }
        Box::pin(async move {
            Ok(attempt
                .issued(
                    AppliedIdentifier::new("fixture-id").unwrap(),
                    Sent::AllRequestAttributes,
                )
                .succeeded(attributes))
        })
    }

    fn resume_create(
        &self,
        _: ResumeCreateRequest,
    ) -> BoxFuture<'_, Result<ResumeOutcome, ResumeFailure>> {
        Box::pin(async { unreachable!("this fixture never resumes creates") })
    }

    fn update(
        &self,
        _: &ResourceId,
        _: &AppliedIdentifier,
        _: UpdateRequest,
        _attempt: carina_core::provider::UpdateAttempt,
    ) -> BoxFuture<'_, ProviderResult<UpdateOutcome>> {
        Box::pin(async { unreachable!("this fixture never updates") })
    }

    fn delete(
        &self,
        _attempt: carina_core::provider::DeleteAttempt,
    ) -> BoxFuture<'_, ProviderResult<()>> {
        Box::pin(async { unreachable!("this fixture never deletes") })
    }

    fn required_permissions(
        &self,
        _: &ResourceId,
        _: carina_core::effect::PlanOp,
    ) -> ProviderResult<Vec<String>> {
        Ok(Vec::new())
    }
}

struct Observer;
impl ExecutionObserver for Observer {
    fn on_event(&self, _: &carina_core::executor::ExecutionEvent) {}
}

fn apply_unknown_output(attributes: &str, output: Value) -> ExecutionResult {
    apply_unknown_outputs(attributes, output, 80)
}

fn apply_unknown_outputs(attributes: &str, output: Value, port: i64) -> ExecutionResult {
    let scenario = Scenario::new(attributes);
    let fixture = scenario.plan();
    assert!(
        fixture.plan.errors().is_empty(),
        "unknown output must defer constraints: {:?}",
        fixture.plan.errors()
    );
    assert!(
        fixture
            .managed_current
            .values()
            .all(|current| matches!(current, Current::Absent(_))),
        "producer must be unknown at plan"
    );
    assert_eq!(fixture.plan.effects().len(), 2);
    let origins: HashMap<_, _> = fixture
        .unresolved_resources
        .iter()
        .map(|origin| {
            (
                carina_core::resource::ResolvedResourceId::new(origin.as_resource().id.clone()),
                origin.clone(),
            )
        })
        .collect();
    let factories: Vec<Box<dyn ProviderFactory>> = vec![Box::new(TypedRefMockFactory)];
    let checkpoints = Arc::new(Mutex::new(Vec::new()));
    let published = Arc::clone(&checkpoints);
    let provider = OutputMock {
        output,
        port,
        creates: Mutex::new(Vec::new()),
    };
    let provider = std::sync::Arc::new(provider);
    let input = LiveExecutionInput {
        plan: &creation_fixture::checked(fixture.plan.clone(), &fixture.schemas),
        provider_check_inputs: ProviderCheckInputs::PlanNormalized {
            resource_origins: &origins,
            data_source_origins: &[],
        },
        compositions: &fixture.compositions,
        bindings: ResolvedBindings::default(),
        managed_current: fixture.managed_current,
        data_source_values: ExecutableDataSourceValues::empty(),
        normalizer: &NoopNormalizer,
        provider_configs: &carina_core::test_utils::resolved_provider_configs(&[]),
        provider_context: carina_core::parser::ProviderContext::new(
            carina_core::EnvironmentSnapshot::empty(),
        ),
        factories: &factories,
        schemas: &fixture.schemas,
        parallelism: carina_cli::DEFAULT_PARALLELISM,
        precompleted_success_count: 0,
        checkpoint_sink: ExecutionCheckpointSink::new(move |result| {
            published.lock().unwrap().push(result)
        }),
    };
    let outcome = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(execute_live_plan_for_test(
            &carina_cli::test_support::boundary(Box::new(std::sync::Arc::clone(&provider))),
            input,
            &Observer,
            carina_core::shutdown::ShutdownToken::running(),
        ));
    let result = match outcome {
        ExecutionOutcome::Completed(result) => result,
        ExecutionOutcome::Cancelled(_) => panic!("fixture apply was unexpectedly cancelled"),
    };
    assert_eq!(
        result.success_count, 1,
        "producer must succeed before checking consumer"
    );
    assert_eq!(result.failure_count(), 1, "{:?}", result.failures());
    let creates = provider.creates.lock().unwrap();
    assert_eq!(
        creates.len(),
        1,
        "consumer must fail before provider create: {creates:?}"
    );
    assert_eq!(creates[0].resource_type, PRODUCER);
    let checkpoints = checkpoints.lock().unwrap();
    let saved = checkpoints
        .last()
        .expect("apply must publish a state checkpoint");
    // New successful creates are handed to the state saver in applied_states;
    // managed_current still describes the pre-apply (Absent) observations.
    for checkpoint in [&result, saved.as_ref()] {
        let Some(Applied::Complete { state, .. }) = checkpoint.applied_states.get(&creates[0])
        else {
            panic!("successful producer must remain checkpointed after consumer failure");
        };
        assert!(state.exists);
        assert_eq!(state.identifier.as_deref(), Some("fixture-id"));
        assert!(state.attributes.contains_key("typed_out"));
        assert!(state.attributes.contains_key("typed_port"));
        assert!(
            !checkpoint
                .applied_states
                .keys()
                .any(|id| id.resource_type == CONSUMER)
        );
    }
    assert!(
        saved
            .managed_current
            .iter()
            .filter(|(id, _)| id.resource_type == CONSUMER)
            .all(|(_, current)| matches!(current, Current::Absent(_)))
    );
    result
}

#[test]
fn typed_ref_apply_rejects_unknown_output_before_consumer_create() {
    for (value, reason) in [("bad-id", "pattern"), ("good-overlongvalue", "length")] {
        for (attributes, path) in [
            ("  x = producer.typed_out", "x"),
            ("  xs = [producer.typed_out]", "xs"),
        ] {
            let result = apply_unknown_output(
                attributes,
                Value::Concrete(ConcreteValue::String(value.into())),
            );
            let error = result.failures()[0].to_string();
            assert!(
                error.contains(&format!("'{path}'")) && error.contains(reason),
                "{error}"
            );
            if path == "xs" {
                assert!(error.contains("index 0"), "{error}");
            }
            assert!(
                !error.contains("cannot assign"),
                "must reach the resolved value gate: {error}"
            );
        }
    }
}

#[test]
fn typed_ref_int_apply_rejects_unknown_out_of_range_output_before_consumer_create() {
    for value in [0, 1025] {
        let result = apply_unknown_outputs(
            "  port = producer.typed_port",
            Value::Concrete(ConcreteValue::String("good-value".into())),
            value,
        );
        let error = result.failures()[0].to_string();
        assert!(error.contains(CONSUMER), "{error}");
        assert!(error.contains("outside allowed range"), "{error}");
        let EffectFailureKind::Execution { error: cause } = result.failures()[0].kind() else {
            panic!("expected a provider preparation failure: {error}");
        };
        let Some(ProviderPreparationError::ValueConstraint { errors, .. }) =
            cause.as_error().downcast_ref::<ProviderPreparationError>()
        else {
            panic!("expected value constraint errors: {error}");
        };
        assert!(
            matches!(
                errors.errors(),
                [TypeError::ResourceValidationFailed { attribute: Some(attribute), .. }]
                    if attribute == "port"
            ),
            "{errors:?}"
        );
        assert!(
            !error.contains("cannot assign"),
            "must reach the resolved value gate: {error}"
        );
    }
}

#[test]
fn typed_ref_secret_constraint_error_does_not_leak_value() {
    for (secret, reason) in [
        ("sensitivebad", "pattern"),
        ("good-privatesensitiveoverlongoutput", "length"),
    ] {
        let result = apply_unknown_output(
            "  x = producer.typed_out",
            Value::Deferred(DeferredValue::Secret(
                Box::new(Value::Concrete(ConcreteValue::String(secret.into()))).into(),
            )),
        );
        let error = result.failures()[0].to_string();
        assert!(error.contains("'x'") && error.contains(reason), "{error}");
        assert!(error.contains("(secret)"), "{error}");
        assert!(
            !error.contains(secret),
            "secret value leaked in constraint error: {error}"
        );
    }
}

#[path = "../../carina-core/tests/support/creation.rs"]
mod creation_fixture;
#[path = "typed_ref_anonymous_refined_sink_e2e/review_round2_4282.rs"]
mod review_round2_4282;
