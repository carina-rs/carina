use super::*;
use carina_core::provider::*;
use carina_core::resource::DeferredValue;

fn string(text: &str) -> Value {
    Value::Concrete(ConcreteValue::String(text.into()))
}

// The first operation produces a value unknown when the provider session is
// minted. The second operation wraps its reference in secret(...).
struct LateEcho(EchoMock);
impl Provider for LateEcho {
    fn name(&self) -> &str {
        "mock"
    }
    fn read(
        &self,
        id: &ResourceId,
        identifier: &AppliedIdentifier,
        request: ReadRequest,
    ) -> BoxFuture<'_, ProviderResult<ReadOutcome>> {
        self.0.read(id, identifier, request)
    }
    fn read_data_source(
        &self,
        resource: &ProviderReadyDataSource,
    ) -> BoxFuture<'_, ProviderResult<State>> {
        self.0.read_data_source(resource)
    }
    fn create(
        &self,
        id: &ResourceId,
        request: CreateRequest,
        attempt: CreateAttempt,
    ) -> BoxFuture<'_, Result<CreateOutcome, CreateFailure>> {
        if request.resource().as_resource().attributes.get("name")
            == Some(&string("deferred-source"))
        {
            return Box::pin(async move {
                Ok(attempt
                    .issued(
                        AppliedIdentifier::new("source-id").unwrap(),
                        Sent::AllRequestAttributes,
                    )
                    .succeeded(HashMap::from([
                        ("name".into(), string("deferred-source")),
                        ("computed".into(), string(SECRET)),
                        (
                            "computed_int".into(),
                            Value::Concrete(ConcreteValue::Int(3)),
                        ),
                        (
                            "computed_bool".into(),
                            Value::Concrete(ConcreteValue::Bool(true)),
                        ),
                    ])))
            });
        }
        self.0.create(id, request, attempt)
    }
    fn resume_create(
        &self,
        request: ResumeCreateRequest,
    ) -> BoxFuture<'_, Result<ResumeOutcome, ResumeFailure>> {
        self.0.resume_create(request)
    }
    fn update(
        &self,
        id: &ResourceId,
        identifier: &AppliedIdentifier,
        request: UpdateRequest,
    ) -> BoxFuture<'_, ProviderResult<UpdateOutcome>> {
        self.0.update(id, identifier, request)
    }
    fn delete(
        &self,
        _: &ResourceId,
        _: &AppliedIdentifier,
        _: DeleteRequest,
    ) -> BoxFuture<'_, ProviderResult<()>> {
        Box::pin(async {
            Err(ProviderError::api_error(format!(
                "mock delete rejected password {SECRET}; public={PUBLIC}"
            )))
        })
    }
    fn required_permissions(
        &self,
        _: &ResourceId,
        _: carina_core::effect::PlanOp,
    ) -> ProviderResult<Vec<String>> {
        Ok(Vec::new())
    }
}

async fn late_execution(failure: Failure) -> ExecutionResult {
    let mut source = resource(PUBLIC, false).as_resource().clone();
    source.id =
        ResourceId::with_provider_identity("mock", "test.resource", "deferred-source", None);
    source.binding = Some(carina_core::resource::absolute_binding_address_for_test(
        "source",
    ));
    source
        .attributes
        .insert("name".into(), string("deferred-source"));
    // Include the output key for the fixture's schema, but its value is not
    // available until LateEcho completes the first create.
    source
        .attributes
        .insert("computed".into(), string("placeholder"));
    let mut desired = resource(PUBLIC, false).as_resource().clone();
    desired.attributes.insert(
        "password".into(),
        Value::Deferred(DeferredValue::Secret(
            Box::new(Value::resource_ref("source", "computed", vec![]).resolve_root_for_test())
                .into(),
        )),
    );
    if failure == Failure::ScalarStatus {
        for (attribute, output) in [("numeric", "computed_int"), ("boolean", "computed_bool")] {
            desired.attributes.insert(
                attribute.into(),
                Value::Deferred(DeferredValue::Secret(
                    Box::new(Value::resource_ref("source", output, vec![]).resolve_root_for_test())
                        .into(),
                )),
            );
        }
    }
    assert!(matches!(
        desired.attributes["password"],
        Value::Deferred(DeferredValue::Secret(_))
    ));
    let provider = fixture::text_fixtures::initialized_for_desired(
        Box::new(LateEcho(EchoMock::new(failure))),
        &[source.clone(), desired.clone()],
    );
    let mut plan = Plan::new();
    let dependency = ResolvedResourceId::new(source.id.clone());
    plan.add(Effect::Create(ResolvedResource::new(source)));
    let desired = ResolvedResource::from_resolved_parts(
        desired,
        std::iter::once(dependency).collect(),
        Default::default(),
    );
    let mut current = HashMap::new();
    if failure == Failure::Update {
        let from = State::<carina_core::resource::NoPlaceholder>::existing(
            desired.id.clone(),
            HashMap::from([
                ("name".into(), string("probe")),
                ("password".into(), string("old")),
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
    } else {
        plan.add(Effect::Create(desired));
    }
    execute(provider.provider(), &plan, current).await
}

#[test]
fn round5_deferred_create_error_uses_resolved_secret() {
    if let Some(stderr) = in_child(
        "round5::round5_deferred_create_error_uses_resolved_secret",
        || {
            tokio::runtime::Runtime::new().unwrap().block_on(async {
                let result = late_execution(Failure::CreateMessage).await;
                assert_eq!(result.success_count, 1);
                assert_eq!(result.failure_count(), 1);
            });
        },
    ) {
        assert!(stderr.contains("mock create rejected password"), "{stderr}");
        assert_redacted(&stderr, SECRET);
    }
}

#[test]
fn round5_deferred_update_error_uses_resolved_secret() {
    if let Some(stderr) = in_child(
        "round5::round5_deferred_update_error_uses_resolved_secret",
        || {
            tokio::runtime::Runtime::new().unwrap().block_on(async {
                let result = late_execution(Failure::Update).await;
                assert_eq!(result.success_count, 1);
                assert_eq!(result.failure_count(), 1);
            });
        },
    ) {
        assert!(stderr.contains("mock update rejected password"), "{stderr}");
        assert_redacted(&stderr, SECRET);
    }
}

#[tokio::test]
async fn round5_deferred_partial_reason_is_redacted_in_persisted_state() {
    let result = late_execution(Failure::PartialMissingKey).await;
    assert_eq!(result.failure_count(), 0);
    assert_eq!(result.partial_count, 1);
    let (
        _,
        carina_core::executor::Applied::Complete {
            state: snapshot, ..
        },
    ) = result
        .applied_states
        .iter()
        .find(|(id, _)| id.identity_str() == Some("probe"))
        .unwrap()
    else {
        panic!("partial create retains its state")
    };
    let mut state = StateFile::new();
    state
        .upsert_resource(
            ResourceState::from_provider_state("test.resource", "probe", "mock", snapshot).unwrap(),
        )
        .unwrap();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("carina.state.json");
    carina_state::backends::local_backend_for_test(path.clone())
        .write_state(&state)
        .await
        .unwrap();
    let text = std::fs::read_to_string(path).unwrap();
    assert!(text.contains("public incomplete read"), "{text}");
    assert_redacted(&text, SECRET);
}

#[test]
fn round5_deferred_scalar_secrets_preserve_structured_status() {
    if let Some(stderr) = in_child(
        "round5::round5_deferred_scalar_secrets_preserve_structured_status",
        || {
            tokio::runtime::Runtime::new().unwrap().block_on(async {
                let result = late_execution(Failure::ScalarStatus).await;
                assert_eq!(result.failure_count(), 1);
            });
        },
    ) {
        assert!(stderr.contains("status: 403"), "{stderr}");
        assert!(stderr.contains("password (secret) (secret)"), "{stderr}");
    }
}

#[test]
fn round5_session_witness_protects_payload_free_delete() {
    if let Some(stderr) = in_child(
        "round5::round5_session_witness_protects_payload_free_delete",
        || {
            tokio::runtime::Runtime::new().unwrap().block_on(async {
                let desired = resource(SECRET, true);
                let provider = fixture::text_fixtures::initialized_for_desired(
                    Box::new(LateEcho(EchoMock::new(Failure::Delete))),
                    std::slice::from_ref(desired.as_resource()),
                );
                let mut plan = Plan::new();
                plan.add(Effect::Delete {
                    id: ResolvedResourceId::new(desired.id.clone()),
                    identifier: AppliedIdentifier::new("delete-id").unwrap(),
                    generation: EffectGeneration::Current,
                    directives: desired.directives.clone(),
                    binding: desired.binding.clone(),
                    dependencies: Default::default(),
                    explicit_dependencies: Default::default(),
                    blocked_by_updates: Default::default(),
                });
                let result = execute(provider.provider(), &plan, HashMap::new()).await;
                assert_eq!(result.failure_count(), 1);
            });
        },
    ) {
        assert!(stderr.contains("mock delete rejected password"), "{stderr}");
        assert_redacted(&stderr, SECRET);
    }
}
