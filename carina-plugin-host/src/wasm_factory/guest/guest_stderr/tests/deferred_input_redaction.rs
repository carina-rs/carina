use super::*;
use carina_core::provider::*;
use carina_core::resource::{ConcreteValue, DeferredValue, Resource, Value};

struct LateFactory(HostOutput);
impl ProviderFactory for LateFactory {
    fn name(&self) -> &str {
        "stderr-test"
    }
    fn display_name(&self) -> &str {
        "Late stderr"
    }
    fn schemas(&self) -> Vec<carina_core::schema::ValidatedResourceSchema> {
        Vec::new()
    }
    fn schema_witness(&self) -> carina_core::schema::ValidatedProviderSchemaWitness {
        StderrConfigFactory.schema_witness()
    }
    fn provider_config_attribute_types(&self) -> carina_core::schema::ValidatedConfigSchema {
        StderrConfigFactory.provider_config_attribute_types()
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
        _: &ProviderSession,
        _: InitializeOutcomeMint,
    ) -> BoxFuture<'_, ProviderResult<ProviderInitialization>> {
        let provider = LateProvider(self.0.clone());
        Box::pin(
            async move { Ok(InitializeOutcome::unavailable().with_provider(Box::new(provider))) },
        )
    }
}
struct LateProvider(HostOutput);
impl Provider for LateProvider {
    fn name(&self) -> &str {
        "stderr-test"
    }
    fn required_permissions(
        &self,
        _: &carina_core::resource::ResourceId,
        _: carina_core::effect::PlanOp,
    ) -> ProviderResult<Vec<String>> {
        Ok(Vec::new())
    }
    fn read(
        &self,
        _: &carina_core::resource::ResourceId,
        _: &AppliedIdentifier,
        _: ReadRequest,
    ) -> BoxFuture<'_, ProviderResult<ReadOutcome>> {
        unreachable!()
    }
    fn read_data_source(
        &self,
        _: &ProviderReadyDataSource,
    ) -> BoxFuture<'_, ProviderResult<carina_core::resource::State>> {
        unreachable!()
    }
    fn create(
        &self,
        _: &carina_core::resource::ResourceId,
        request: CreateRequest,
        attempt: CreateAttempt,
    ) -> BoxFuture<'_, Result<CreateOutcome, CreateFailure>> {
        let wire = request
            .resource()
            .wire_attributes()
            .get("password")
            .unwrap();
        let plaintext = serde_json::to_value(wire.value())
            .unwrap()
            .as_str()
            .unwrap()
            .to_owned();
        self.0.write_line(&format!("during {plaintext}")).unwrap();
        Box::pin(
            async move { Err(attempt.not_created(ProviderError::api_error("expected failure"))) },
        )
    }
    fn resume_create(
        &self,
        _: ResumeCreateRequest,
    ) -> BoxFuture<'_, Result<ResumeOutcome, ResumeFailure>> {
        unreachable!()
    }
    fn update(
        &self,
        _: &carina_core::resource::ResourceId,
        _: &AppliedIdentifier,
        _: UpdateRequest,
        _: UpdateAttempt,
    ) -> BoxFuture<'_, ProviderResult<UpdateOutcome>> {
        unreachable!()
    }
    fn delete(&self, _: DeleteAttempt) -> BoxFuture<'_, ProviderResult<()>> {
        unreachable!()
    }
}

#[tokio::test]
async fn guest_stderr_round5_deferred_ref_grows_during_and_after_create() {
    use carina_core::executor::*;
    const SECRET: &str = "late-guest-private-4305";
    let capture = Capture::default();
    let stderr = HostOutput::new(capture.clone());
    let mut desired = Resource::with_provider("stderr-test", "resource", "probe", None);
    desired.attributes.insert(
        "password".into(),
        Value::Deferred(DeferredValue::Secret(
            Box::new(Value::resource_ref("source", "computed", vec![]).resolve_root_for_test())
                .into(),
        )),
    );
    let session =
        plan_for_desired(std::slice::from_ref(&desired)).test_session("stderr-test", None, None);
    let provider = ProviderBoundary::initialize_for_test(&LateFactory(stderr.clone()), &session)
        .await
        .unwrap();
    let _writer = stderr.begin_call(&session.guest_inputs());
    // The value becomes available after minting, just as a successful upstream
    // create publishes its provider-populated attributes into the bindings.
    let mut bindings = carina_core::binding_index::ResolvedBindings::default();
    bindings.set(
        "source",
        std::collections::HashMap::from([(
            "computed".into(),
            Value::Concrete(ConcreteValue::String(SECRET.into())),
        )]),
        carina_core::binding_index::BindingValueSource::Local,
    );
    let mut plan = carina_core::plan::Plan::new();
    plan.add(carina_core::effect::Effect::Create(
        carina_core::resource::ResolvedResource::new(
            desired.into_resolved_dependency_phase_for_test(),
        ),
    ));
    let schemas = carina_core::schema::SchemaRegistry::new();
    let plan = carina_core::input_rules::finalize_plan_for_test(plan)
        .check_inputs(&schemas, &Default::default())
        .unwrap();
    let input = LiveExecutionInput {
        plan: &plan,
        provider_check_inputs: ProviderCheckInputs::Authored,
        compositions: &[],
        bindings,
        managed_current: Default::default(),
        data_source_values: carina_core::data_source_value::ExecutableDataSourceValues::empty(),
        normalizer: &NoopNormalizer,
        provider_configs: &carina_core::test_utils::resolved_provider_configs(&[]),
        provider_context: carina_core::parser::ProviderContext::new(
            carina_core::EnvironmentSnapshot::empty(),
        ),
        factories: &[],
        schemas: &schemas,
        parallelism: std::num::NonZeroUsize::new(1).unwrap(),
        precompleted_success_count: 0,
        checkpoint_sink: ExecutionCheckpointSink::new(|_| {}),
    };
    let ExecutionOutcome::Completed(result) = execute_live_plan_for_test(
        provider.provider(),
        input,
        &NoopExecutionObserver,
        carina_core::shutdown::ShutdownToken::running(),
    )
    .await
    else {
        panic!("execution cancelled")
    };
    assert_eq!(result.failure_count(), 1);
    stderr.write_line(&format!("after {SECRET}")).unwrap();
    stderr.flush_pending().unwrap();
    let text = capture.text();
    assert!(
        !text.contains(SECRET),
        "guest stderr leaked a resolved secret: {text}"
    );
    assert!(
        text.contains("during (secret)") && text.contains("after (secret)"),
        "{text}"
    );
}
