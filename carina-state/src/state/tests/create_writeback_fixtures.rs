//! State writeback fixtures obtain host records from the real dispatch boundary.

use super::*;
use carina_core::provider::*;
use carina_core::resource::NoPlaceholder;
use carina_core::schema::ValidatedResourceSchema;

pub(super) fn create_request_for_test(
    resource: Resource,
    schema: ValidatedResourceSchema,
) -> CreateRequest {
    let bindings = carina_core::binding_index::ResolvedBindings::default();
    let gate = carina_core::executor::ModuleConstraintGate::from_live_compositions(&[]);
    let mut schemas = carina_core::schema::SchemaRegistry::new();
    schemas.insert(&resource.id.provider, schema);
    carina_core::validation::deferred_types::with_session_for_test(
        &carina_core::parser::LoadedProviderContext::empty_for_test(),
        |type_checks| {
            let preparation = carina_core::executor::ProviderPreparationContext::for_test(
                &bindings,
                &gate,
                &[],
                &NoopNormalizer,
                &schemas,
                type_checks,
            );
            ready_for_test(carina_core::executor::prepare_create_request(
                resource,
                &preparation,
            ))
            .expect("fixture passes provider preparation")
        },
    )
}

fn request_for_resource<S, D>(
    resource: &Resource<S, NoPlaceholder, AbsoluteBindingAddress, D>,
    schema: Option<ValidatedResourceSchema>,
) -> CreateRequest {
    // Fixtures without an explicit schema have only scalar string inputs.
    let schema = schema.unwrap_or_else(|| {
        resource
            .attributes
            .keys()
            .fold(
                PatternResourceSchema::new(&resource.id.resource_type),
                |schema, key| {
                    schema.attribute(
                        PatternAttributeSchema::optional(key, PatternAttributeType::string())
                            .build(),
                    )
                },
            )
            .validate()
            .expect("fixture schema must validate")
    });
    let mut input = Resource::from_id(resource.id.clone());
    input.attributes = resource.attributes.clone();
    create_request_for_test(input, schema)
}

pub(super) fn ready_for_test<F: std::future::Future>(future: F) -> F::Output {
    use std::task::{Context, Poll, Waker};

    let mut future = std::pin::pin!(future);
    let mut context = Context::from_waker(Waker::noop());
    match future.as_mut().poll(&mut context) {
        Poll::Ready(output) => output,
        Poll::Pending => panic!("fixture operation must complete synchronously"),
    }
}

fn fixture_provider<C, R>(name: String, create: C, resume: R) -> impl Provider
where
    C: Fn(CreateAttempt) -> BoxFuture<'static, Result<CreateOutcome, CreateFailure>>
        + Send
        + Sync
        + 'static,
    R: Fn(ResumeCreateRequest) -> BoxFuture<'static, Result<ResumeOutcome, ResumeFailure>>
        + Send
        + Sync
        + 'static,
{
    struct WritebackFixture<C, R> {
        name: String,
        create: C,
        resume: R,
    }

    impl<C, R> Provider for WritebackFixture<C, R>
    where
        C: Fn(CreateAttempt) -> BoxFuture<'static, Result<CreateOutcome, CreateFailure>>
            + Send
            + Sync,
        R: Fn(ResumeCreateRequest) -> BoxFuture<'static, Result<ResumeOutcome, ResumeFailure>>
            + Send
            + Sync,
    {
        fn name(&self) -> &str {
            &self.name
        }
        fn read(
            &self,
            _: &ResourceId,
            _: &AppliedIdentifier,
            _: ReadRequest,
        ) -> BoxFuture<'_, ProviderResult<ReadOutcome>> {
            panic!("unexpected fixture read")
        }
        fn read_data_source(
            &self,
            _: &ProviderReadyDataSource,
        ) -> BoxFuture<'_, ProviderResult<State>> {
            panic!("unexpected fixture data-source read")
        }
        fn create(
            &self,
            _: &ResourceId,
            _: CreateRequest,
            attempt: CreateAttempt,
        ) -> BoxFuture<'_, Result<CreateOutcome, CreateFailure>> {
            (self.create)(attempt)
        }
        fn resume_create(
            &self,
            request: ResumeCreateRequest,
        ) -> BoxFuture<'_, Result<ResumeOutcome, ResumeFailure>> {
            (self.resume)(request)
        }
        fn update(
            &self,
            _: &ResourceId,
            _: &AppliedIdentifier,
            _: UpdateRequest,
            _: UpdateAttempt,
        ) -> BoxFuture<'_, ProviderResult<UpdateOutcome>> {
            panic!("unexpected fixture update")
        }
        fn delete(&self, _: DeleteAttempt) -> BoxFuture<'_, ProviderResult<()>> {
            panic!("unexpected fixture delete")
        }
        fn required_permissions(
            &self,
            _: &ResourceId,
            _: carina_core::effect::PlanOp,
        ) -> ProviderResult<Vec<String>> {
            panic!("unexpected fixture permissions")
        }
    }

    WritebackFixture {
        name,
        create,
        resume,
    }
}

pub(super) fn create_fixture_provider(
    name: impl Into<String>,
    create: impl Fn(CreateAttempt) -> BoxFuture<'static, Result<CreateOutcome, CreateFailure>>
    + Send
    + Sync
    + 'static,
) -> impl Provider {
    fixture_provider(name.into(), create, |_| panic!("unexpected fixture resume"))
}

// Capture an attempt only after a real provider-ready request has passed
// admission and discharge. This helper is private to state writeback tests.
pub(super) fn create_attempt_for_test(id: ResourceId) -> CreateAttempt {
    use std::future::Future;
    use std::task::{Context, Waker};

    let captured = std::sync::Arc::new(std::sync::Mutex::new(None));
    let capture = std::sync::Arc::clone(&captured);
    let provider = create_fixture_provider(id.provider.clone(), move |attempt| {
        *capture.lock().unwrap() = Some(attempt);
        Box::pin(std::future::pending())
    });
    let request = create_request_for_test(
        Resource::from_id(id.clone()),
        PatternResourceSchema::new(&id.resource_type)
            .validate()
            .unwrap(),
    );
    let prepared = carina_core::replacement::create_permit_for_test(request);
    let boundary = crate::provider_text_fixtures::boundary(Box::new(provider));
    let (create, _cancellation) = carina_core::executor::dispatch_create(&boundary, prepared);
    let mut create = std::pin::pin!(create);
    assert!(
        create
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    captured.lock().unwrap().take().unwrap()
}

pub(super) fn dispatched_create_failure_for_test<S, D>(
    resource: &Resource<S, NoPlaceholder, AbsoluteBindingAddress, D>,
    failure: CreateFailure,
    schema: Option<ValidatedResourceSchema>,
) -> HostCreateFailure {
    let failure = std::sync::Mutex::new(Some(failure));
    let provider = create_fixture_provider(resource.id.provider.clone(), move |_| {
        Box::pin(std::future::ready(Err(failure
            .lock()
            .unwrap()
            .take()
            .unwrap())))
    });
    let boundary = crate::provider_text_fixtures::initialized_for_desired(
        Box::new(provider),
        std::slice::from_ref(resource),
    )
    .into_boundary();
    let request = request_for_resource(resource, schema);
    let permit = carina_core::replacement::create_permit_for_test(request);
    let (create, _cancellation) = carina_core::executor::dispatch_create(&boundary, permit);
    let Err(failure) = ready_for_test(create) else {
        panic!("fixture must return a classified create failure")
    };
    failure
}

pub(super) fn resumed_create_observation_for_test(
    resource: &Resource,
    pending: InFlightCreate,
    observation: AppliedState,
    schema: Option<ValidatedResourceSchema>,
) -> HostIdentifiedCreate {
    let observation = std::sync::Mutex::new(Some(observation));
    let provider = fixture_provider(
        resource.id.provider.clone(),
        |_| panic!("unexpected fixture create"),
        move |request| {
            let (Resume::Issued(resumed), _request) = request.into_parts() else {
                panic!("fixture continuation must have a provider identifier")
            };
            let observation = Observation::Read(ReadOutcome::found(
                observation.lock().unwrap().take().unwrap(),
            ));
            Box::pin(std::future::ready(Err(resumed.identifier_reported(
                observation,
                ProviderError::internal("synthetic resumed-create finishing failure"),
            ))))
        },
    );
    let boundary = crate::provider_text_fixtures::initialized_for_desired(
        Box::new(provider),
        std::slice::from_ref(resource),
    )
    .into_boundary();
    let request = ResumeCreateRequest::for_test(pending, request_for_resource(resource, schema))
        .expect("fixture continuation and request must share a route");
    let Err(failure) = ready_for_test(carina_core::executor::dispatch_resume_create(
        &boundary, request,
    )) else {
        panic!("fixture must return a resumed observation")
    };
    let ResumeFailureKind::IdentifierReported { identified, .. } = failure.into_kind() else {
        panic!("resumed fixture must retain its provider identifier")
    };
    identified
}
