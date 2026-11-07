//! carina#4233 Stage B: owner-authorized edges across directory and execution boundaries.

use std::collections::HashMap;
use std::fs;
use std::sync::Mutex;

use carina_cli::fixture_plan::{FixturePlan, build_plan_from_fixture_path_using};
use carina_cli::test_support::{TestCommandRuntime, run_plan_with_factories};
use carina_cli::wiring::LoadedProviderFactory;
use carina_core::binding_index::ResolvedBindings;
use carina_core::data_source_value::ExecutableDataSourceValues;
use carina_core::executor::{
    ExecutionCheckpointSink, ExecutionObserver, ExecutionOutcome, LiveExecutionInput,
    ProviderCheckInputs, execute_live_plan_for_test,
};
use carina_core::plan::PlanErrorKind;
use carina_core::provider::{
    AppliedIdentifier, BoxFuture, CreateAttempt, CreateFailure, CreateOutcome, CreateRequest,
    NoopNormalizer, Provider, ProviderFactory, ProviderReadyDataSource, ProviderResult,
    ReadOutcome, ReadRequest, ResumeCreateRequest, ResumeFailure, ResumeOutcome, Sent,
    UpdateOutcome, UpdateRequest,
};
use carina_core::resource::{ConcreteValue, Current, ResourceId, State, Value};
use carina_core::schema::{
    AttributeSchema, AttributeType, ResourceSchema, TypeIdentity, ValidatedConfigSchema,
    ValidatedProviderSchemaWitness, ValidatedResourceSchema,
};
use carina_state::{ResourceState, StateFile};
use tempfile::TempDir;

#[path = "../../carina-core/tests/support/identity_edges.rs"]
pub mod identity_edges;

struct RuntimeEdgeFactory;
impl ProviderFactory for RuntimeEdgeFactory {
    fn name(&self) -> &str {
        "mock"
    }
    fn display_name(&self) -> &str {
        "Identity edge fixture"
    }
    fn schemas(&self) -> Vec<ValidatedResourceSchema> {
        let mut schemas = identity_edges::EdgeFactory {
            owner: "mock",
            edges: true,
        }
        .schemas();
        schemas.push(
            ResourceSchema::new("Receiver")
                .attribute(
                    AttributeSchema::optional("bucket", identity_edges::string("Name")).build(),
                )
                .attribute(
                    AttributeSchema::optional(
                        "buckets",
                        AttributeType::list(identity_edges::string("Name")),
                    )
                    .build(),
                )
                .attribute(
                    AttributeSchema::optional(
                        "bucket_map",
                        AttributeType::map(identity_edges::string("Name")),
                    )
                    .build(),
                )
                .validate()
                .unwrap(),
        );
        schemas.push(
            ResourceSchema::new("CollectionProducer")
                .attribute(
                    AttributeSchema::provider_populated(
                        "new_buckets",
                        AttributeType::list(identity_edges::string("NewName")),
                    )
                    .build(),
                )
                .attribute(
                    AttributeSchema::provider_populated(
                        "new_bucket_map",
                        AttributeType::map(identity_edges::string("NewName")),
                    )
                    .build(),
                )
                .validate()
                .unwrap(),
        );
        schemas
    }
    fn schema_witness(&self) -> ValidatedProviderSchemaWitness {
        let edges = identity_edges::EdgeFactory {
            owner: "mock",
            edges: true,
        }
        .schema_witness();
        ValidatedProviderSchemaWitness::new(
            self.schemas(),
            self.provider_config_attribute_types(),
            Default::default(),
            edges.assignability_edges().to_vec(),
        )
        .unwrap()
    }
    fn provider_config_attribute_types(&self) -> ValidatedConfigSchema {
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
                        creates: Mutex::new(Vec::new()),
                    },
                )),
            )
        })
    }
}
fn factories() -> Vec<LoadedProviderFactory> {
    LoadedProviderFactory::for_test(vec![
        Box::new(RuntimeEdgeFactory),
        Box::new(identity_edges::EdgeFactory {
            owner: "other",
            edges: false,
        }),
    ])
}
struct Scenario {
    project: TempDir,
}
impl Scenario {
    fn new(attributes: &str, owner: &str) -> Self {
        let project = tempfile::tempdir().unwrap();
        for (file, source) in [
            (
                "providers.crn",
                format!(
                    "backend local {{}}\nprovider mock {{}}\n{}",
                    if owner == "other" {
                        "provider other {}\n"
                    } else {
                        ""
                    }
                ),
            ),
            ("producer.crn", "let producer = mock.Bucket {}\n".into()),
            (
                "consumer.crn",
                format!("let consumer = {owner}.Bucket {{\n{attributes}\n}}\n"),
            ),
        ] {
            fs::write(project.path().join(file), source).unwrap();
        }
        Self { project }
    }
    fn validate(&self) -> Vec<String> {
        carina_cli::commands::validate::validate_with_factories(
            &carina_core::paths::ResolvedPath::new(self.project.path()).unwrap(),
            factories(),
        )
    }
    fn plan(&self) -> FixturePlan {
        build_plan_from_fixture_path_using(&TestCommandRuntime::new(), self.project.path(), |_| {
            factories()
        })
        .unwrap()
    }
    fn seed(&self, text: &str) {
        let mut producer = ResourceState::try_applied("Bucket", "producer", "mock", "producer-id")
            .unwrap()
            .with_attribute("new_name", serde_json::json!(text));
        producer.binding = Some(carina_core::resource::absolute_binding_address_for_test(
            "producer",
        ));
        let mut state = StateFile::new();
        state.upsert_resource(producer).unwrap();
        fs::write(
            self.project.path().join("carina.state.json"),
            carina_core::utils::pretty_with_newline(&state).unwrap(),
        )
        .unwrap();
    }
}
#[test]
fn identity_edges_validate_forward_and_reject_reverse_and_unrelated() {
    let errors = Scenario::new("bucket = producer.new_name", "mock").validate();
    assert!(errors.is_empty(), "{errors:#?}");
    for (attributes, mismatch) in [
        (
            "new_bucket = producer.old_name",
            "expected mock.Bucket.NewName, got mock.Bucket.Name",
        ),
        (
            "bucket = producer.unrelated",
            "expected mock.Bucket.Name, got mock.Bucket.Other",
        ),
    ] {
        let errors = Scenario::new(attributes, "mock").validate();
        assert_eq!(errors.len(), 1, "{errors:#?}");
        assert!(
            errors[0].contains("cannot assign") && errors[0].contains(mismatch),
            "{errors:#?}"
        );
    }
}
#[test]
fn identity_edges_do_not_use_a_co_owners_edge_at_another_factorys_attribute() {
    let errors = Scenario::new("bucket = producer.new_name", "other").validate();
    assert_eq!(errors.len(), 1, "{errors:#?}");
    assert!(
        errors[0].contains("cannot assign")
            && errors[0].contains("expected mock.Bucket.Name, got mock.Bucket.NewName"),
        "{errors:#?}"
    );
}
fn module_scenario(owner: &str) -> Scenario {
    // The second factory does not co-own Name: the annotation may use mock's
    // edge, but the later provider sink must use its own factory's authority.
    let scenario = Scenario::new("", "other");
    fs::write(scenario.project.path().join("consumer.crn"), "let child = use { source = './child' }\nlet instance = child { input = producer.new_name }\n").unwrap();
    fs::create_dir(scenario.project.path().join("child")).unwrap();
    fs::write(
        scenario.project.path().join("child/arguments.crn"),
        "arguments { input: mock.Bucket.Name }\n",
    )
    .unwrap();
    fs::write(
        scenario.project.path().join("child/resources.crn"),
        format!("let consumer = {owner}.Receiver {{ bucket = input }}\n"),
    )
    .unwrap();
    scenario
}
struct ForwardFactory;
impl ProviderFactory for ForwardFactory {
    fn name(&self) -> &str {
        "other"
    }
    fn display_name(&self) -> &str {
        "Other identity sink"
    }
    fn schemas(&self) -> Vec<ValidatedResourceSchema> {
        let name =
            || AttributeType::refined_string(Some(TypeIdentity::bare("Name")), None, None, None);
        vec![
            ResourceSchema::new("Receiver")
                .attribute(AttributeSchema::optional("bucket", name()).build())
                .attribute(
                    AttributeSchema::optional("buckets", AttributeType::list(name())).build(),
                )
                .attribute(
                    AttributeSchema::optional("bucket_map", AttributeType::map(name())).build(),
                )
                .validate()
                .unwrap(),
        ]
    }
    fn schema_witness(&self) -> ValidatedProviderSchemaWitness {
        ValidatedProviderSchemaWitness::new(
            self.schemas(),
            Default::default(),
            Default::default(),
            Vec::new(),
        )
        .unwrap()
    }
    fn provider_config_attribute_types(&self) -> ValidatedConfigSchema {
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
        Box::pin(async { unreachable!("validation only") })
    }
}
// The annotation's mock.Bucket.Name is owned only by mock. The later Q sink
// has the structurally wider Name identity. Replacing the actual NewName with
// the formal Name would therefore leak P's edge into Q's attribute.
#[test]
fn identity_edges_module_argument_retains_actual_identity_at_later_sink() {
    for (owner, accepted) in [("mock", true), ("other", false)] {
        let scenario = module_scenario(owner);
        let errors = carina_cli::commands::validate::validate_with_factories(
            &carina_core::paths::ResolvedPath::new(scenario.project.path()).unwrap(),
            LoadedProviderFactory::for_test(vec![
                Box::new(RuntimeEdgeFactory),
                Box::new(ForwardFactory),
            ]),
        );
        if accepted {
            assert!(errors.is_empty(), "{errors:#?}");
        } else {
            assert!(
                errors
                    .iter()
                    .any(|error| error.contains("got mock.Bucket.NewName")
                        && error.contains("child/resources.crn")),
                "{errors:#?}"
            );
        }
    }
}

fn collection_module_errors(owner: &str, collection: &str, attribute: &str) -> Vec<String> {
    // The call admits NewName through mock's edge into the Name declaration.
    // Each later sink must re-admit the original collection under its own owner.
    let scenario = module_scenario(owner);
    fs::write(
        scenario.project.path().join("producer.crn"),
        "let producer = mock.CollectionProducer {}\n",
    )
    .unwrap();
    fs::write(
        scenario.project.path().join("consumer.crn"),
        format!("let child = use {{ source = './child' }}\nlet instance = child {{ input = producer.new_{attribute} }}\n"),
    )
    .unwrap();
    fs::write(
        scenario.project.path().join("child/arguments.crn"),
        format!("arguments {{ input: {collection}(mock.Bucket.Name) }}\n"),
    )
    .unwrap();
    fs::write(
        scenario.project.path().join("child/resources.crn"),
        format!("let consumer = {owner}.Receiver {{ {attribute} = input }}\n"),
    )
    .unwrap();
    carina_cli::commands::validate::validate_with_factories(
        &carina_core::paths::ResolvedPath::new(scenario.project.path()).unwrap(),
        LoadedProviderFactory::for_test(vec![
            Box::new(RuntimeEdgeFactory),
            Box::new(ForwardFactory),
        ]),
    )
}

#[test]
fn identity_edges_list_module_argument_rejects_a_later_owner_without_the_edge() {
    let errors = collection_module_errors("other", "list", "buckets");
    assert_eq!(errors.len(), 1, "{errors:#?}");
    assert!(
        errors[0].contains("Type mismatch: expected List<Name>, got List<mock.Bucket.NewName>")
            && errors[0].contains("child/resources.crn"),
        "{errors:#?}"
    );
}

#[test]
fn identity_edges_list_module_argument_accepts_the_edge_declaring_owner() {
    let errors = collection_module_errors("mock", "list", "buckets");
    assert!(errors.is_empty(), "{errors:#?}");
}

#[test]
fn identity_edges_map_module_argument_rejects_a_later_owner_without_the_edge() {
    let errors = collection_module_errors("other", "map", "bucket_map");
    assert_eq!(errors.len(), 1, "{errors:#?}");
    assert!(
        errors[0].contains("Type mismatch: expected Map<Name>, got Map<mock.Bucket.NewName>")
            && errors[0].contains("child/resources.crn"),
        "{errors:#?}"
    );
}

#[test]
fn identity_edges_map_module_argument_accepts_the_edge_declaring_owner() {
    let errors = collection_module_errors("mock", "map", "bucket_map");
    assert!(errors.is_empty(), "{errors:#?}");
}

#[test]
fn identity_edges_plan_runs_the_sink_identity_validator() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let scenario = Scenario::new("bucket = producer.new_name", "mock");
    for (text, valid) in [("bad-value", false), ("good-value", true)] {
        scenario.seed(text);
        carina_state::BackendLock::local_default()
            .save(&carina_core::paths::ResolvedPath::new(scenario.project.path()).unwrap())
            .unwrap();
        let result = runtime.block_on(run_plan_with_factories(
            &TestCommandRuntime::new(),
            scenario.project.path(),
            factories(),
        ));
        if valid {
            assert!(result.unwrap());
        } else {
            let error = result.unwrap_err();
            assert!(
                matches!(&error, carina_cli::error::AppError::Validation(message) if message == "plan failed with 1 error(s)"),
                "{error:#?}"
            );
            let fixture = scenario.plan();
            assert!(
                matches!(&fixture.plan.errors()[0].kind, PlanErrorKind::ResolvedValueConstraint { message, attributes: _, origins: _ } if message.as_str().contains("Name must start with good-"))
            );
        }
    }
}
struct OutputMock {
    output: Value,
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
        if id.identity_str() == Some("producer") {
            attributes.insert("new_name".to_string(), self.output.clone());
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
#[test]
fn identity_edges_apply_checks_unknown_value_before_consumer_create() {
    let scenario = Scenario::new("bucket = producer.new_name", "mock");
    let validation = scenario.validate();
    assert!(
        validation.is_empty(),
        "edge must pass static checking: {validation:#?}"
    );
    let fixture = scenario.plan();
    assert!(
        fixture.plan.errors().is_empty(),
        "{:?}",
        fixture.plan.errors()
    );
    assert!(
        fixture
            .managed_current
            .values()
            .all(|current| matches!(current, Current::Absent(_)))
    );
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
    let factories: Vec<Box<dyn ProviderFactory>> = vec![Box::new(RuntimeEdgeFactory)];
    let provider = OutputMock {
        output: Value::Concrete(ConcreteValue::String("bad-value".into())),
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
        provider_configs: &[],
        provider_context: carina_core::parser::ProviderContext::new(
            carina_core::EnvironmentSnapshot::empty(),
        ),
        factories: &factories,
        schemas: &fixture.schemas,
        parallelism: carina_cli::DEFAULT_PARALLELISM,
        precompleted_success_count: 0,
        checkpoint_sink: ExecutionCheckpointSink::new(|_| {}),
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
        ExecutionOutcome::Cancelled(_) => panic!("cancelled"),
    };
    assert_eq!(result.success_count, 1);
    assert_eq!(result.failure_count(), 1);
    assert!(
        result.failures()[0]
            .to_string()
            .contains("Name must start with good-")
    );
    let creates = provider.creates.lock().unwrap();
    assert_eq!(
        creates.len(),
        1,
        "consumer must fail before create: {creates:?}"
    );
    assert_eq!(creates[0].identity_str(), Some("producer"));
}

#[test]
fn identity_edges_use_the_data_source_input_owner() {
    for (owner, attribute, source, mismatch) in [
        ("mock", "bucket", "new_name", None),
        (
            "mock",
            "new_bucket",
            "old_name",
            Some("expected mock.Bucket.NewName, got mock.Bucket.Name"),
        ),
        (
            "mock",
            "bucket",
            "unrelated",
            Some("expected mock.Bucket.Name, got mock.Bucket.Other"),
        ),
        (
            "other",
            "bucket",
            "new_name",
            Some("expected mock.Bucket.Name, got mock.Bucket.NewName"),
        ),
    ] {
        let scenario = Scenario::new("", owner);
        let required_bucket = if attribute == "new_bucket" {
            "bucket = producer.old_name\n"
        } else {
            ""
        };
        fs::write(
            scenario.project.path().join("consumer.crn"),
            format!("let consumer = read {owner}.Lookup {{ {required_bucket}{attribute} = producer.{source} }}\n"),
        )
        .unwrap();
        let errors = scenario.validate();
        if let Some(mismatch) = mismatch {
            assert_eq!(errors.len(), 1, "{errors:#?}");
            assert!(
                errors[0].contains("cannot assign") && errors[0].contains(mismatch),
                "{errors:#?}"
            );
        } else {
            assert!(errors.is_empty(), "{errors:#?}");
        }
    }
}

#[path = "../../carina-core/tests/support/creation.rs"]
mod creation_fixture;
