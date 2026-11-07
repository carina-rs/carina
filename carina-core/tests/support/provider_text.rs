#![allow(dead_code)]
use carina_core::provider::*;
use carina_core::provider_text::*;
use carina_core::resource::{ConcreteValue, DeferredValue, Resource, ResourceId, State, Value};
use carina_core::schema::{
    ValidatedConfigSchema, ValidatedProviderSchemaWitness, ValidatedResourceSchema,
};
use std::sync::Mutex;

pub fn plan_for_desired<S, D>(
    desired: &[Resource<
        S,
        carina_core::resource::NoPlaceholder,
        carina_core::resource::AbsoluteBindingAddress,
        D,
    >],
) -> ProviderPlan {
    let mut file = carina_core::parser::ExpandedFile::<()>::default();
    file.resources = desired
        .iter()
        .map(|resource| {
            let mut input = Resource::from_id(resource.id.clone());
            input.attributes = resource.attributes.clone();
            input
        })
        .collect();
    carina_core::validation::ResolvedFile::for_test(file).provider_plan()
}

fn fixture_session<S, D>(
    provider: &str,
    binding: Option<&str>,
    config: Option<&ProviderReadyConfig>,
    desired: &[Resource<
        S,
        carina_core::resource::NoPlaceholder,
        carina_core::resource::AbsoluteBindingAddress,
        D,
    >],
    _: &[carina_core::resource::DataSource],
) -> ProviderSession {
    plan_for_desired(desired).session(provider, binding, config)
}

pub fn block_on<T>(future: impl std::future::Future<Output = T>) -> T {
    struct Wake(std::thread::Thread);
    impl std::task::Wake for Wake {
        fn wake(self: std::sync::Arc<Self>) {
            self.0.unpark();
        }
    }
    let waker = std::task::Waker::from(std::sync::Arc::new(Wake(std::thread::current())));
    let mut context = std::task::Context::from_waker(&waker);
    let mut future = std::pin::pin!(future);
    loop {
        match future.as_mut().poll(&mut context) {
            std::task::Poll::Ready(value) => return value,
            std::task::Poll::Pending => std::thread::park(),
        }
    }
}

#[derive(Clone)]
pub struct Context {
    desired: Resource,
    session: ProviderSession,
}
pub fn context(values: &[&str]) -> Context {
    let mut desired = Resource::with_provider("text-fixture", "resource", "fixture", None);
    for (index, value) in values.iter().enumerate() {
        desired.attributes.insert(
            format!("password_{index}"),
            Value::Deferred(DeferredValue::Secret(
                Box::new(Value::Concrete(ConcreteValue::String((*value).into()))).into(),
            )),
        );
    }
    let session = fixture_session(
        "text-fixture",
        None,
        None,
        std::slice::from_ref(&desired),
        &[],
    );
    Context { session, desired }
}
impl Context {
    pub fn session(&self) -> &ProviderSession {
        &self.session
    }
}

pub fn rebind_state_only(error: ProviderError) -> HostError {
    let context = Context {
        desired: Resource::with_provider("text-fixture", "resource", "fixture", None),
        session: ProviderPlan::state_only().session("text-fixture", None, None),
    };
    error.with_secrets_for_test(&context)
}

struct NativeFactory {
    scope: ReferenceScope,
    normalizer: Mutex<Option<Box<dyn ProviderNormalizer>>>,
    provider: Mutex<Option<Box<dyn Provider>>>,
    schemas: Vec<ValidatedResourceSchema>,
    name: String,
}
impl ProviderFactory for NativeFactory {
    fn name(&self) -> &str {
        &self.name
    }
    fn display_name(&self) -> &str {
        "Text fixture"
    }
    fn validate_config(
        &self,
        _: &CanonicalProviderConfigAttributes,
        _: &ProviderValidationInputs,
    ) -> ProviderResult<()> {
        Ok(())
    }
    fn provider_config_attribute_types(&self) -> ValidatedConfigSchema {
        ValidatedConfigSchema::default()
    }
    fn schemas(&self) -> Vec<ValidatedResourceSchema> {
        self.schemas.clone()
    }
    fn schema_witness(&self) -> ValidatedProviderSchemaWitness {
        ValidatedProviderSchemaWitness::new(
            self.schemas.clone(),
            self.provider_config_attribute_types(),
            Default::default(),
            Vec::new(),
        )
        .unwrap()
    }
    fn create_provider(
        &self,
        _: &ProviderSession,
        mint: InitializeOutcomeMint,
    ) -> BoxFuture<'_, ProviderResult<ProviderInitialization>> {
        let provider = self.provider.lock().unwrap().take().unwrap();
        let outcome = match &self.scope {
            ReferenceScope::Unavailable => InitializeOutcome::unavailable(),
            ReferenceScope::Available(values) => mint
                .available(values.partition(), values.region(), values.account())
                .unwrap(),
        };
        Box::pin(async move { Ok(outcome.with_provider(provider)) })
    }
    fn create_normalizer(
        &self,
        _: &ProviderSession,
    ) -> BoxFuture<'_, ProviderResult<Box<dyn ProviderNormalizer>>> {
        let normalizer = self
            .normalizer
            .lock()
            .unwrap()
            .take()
            .unwrap_or_else(|| Box::new(NoopNormalizer));
        Box::pin(async { Ok(normalizer) })
    }
}
pub fn initialized(provider: Box<dyn Provider>) -> InitializedProvider {
    initialized_with_context(provider, &context(&[]))
}
pub fn initialized_with_context(
    provider: Box<dyn Provider>,
    context: &Context,
) -> InitializedProvider {
    block_on(ProviderBoundary::initialize(
        &NativeFactory {
            scope: ReferenceScope::Unavailable,
            normalizer: Mutex::new(None),
            provider: Mutex::new(Some(provider)),
            schemas: Vec::new(),
            name: "text-fixture".into(),
        },
        context.session(),
    ))
    .unwrap()
}

// Values enter through actual provider operations, using the immutable session.
enum Payload {
    Error(ProviderError),
    Create(Result<CreateOutcome, CreateFailure>),
    Update(UpdateOutcome),
    Resume(Result<ResumeOutcome, ResumeFailure>),
    Partial(PartialReadDiagnostic),
}
struct Fixture(Mutex<Option<Payload>>);
impl Provider for Fixture {
    fn required_permissions(
        &self,
        _: &ResourceId,
        _: carina_core::effect::PlanOp,
    ) -> ProviderResult<Vec<String>> {
        Ok(Vec::new())
    }
    fn name(&self) -> &str {
        "text-fixture"
    }
    fn read(
        &self,
        _: &ResourceId,
        _: &AppliedIdentifier,
        _: ReadRequest,
    ) -> BoxFuture<'_, ProviderResult<ReadOutcome>> {
        let Some(Payload::Error(error)) = self.0.lock().unwrap().take() else {
            panic!("error fixture")
        };
        Box::pin(async { Err(error) })
    }
    fn read_data_source(
        &self,
        _: &ProviderReadyDataSource,
    ) -> BoxFuture<'_, ProviderResult<State>> {
        unreachable!()
    }
    fn create(
        &self,
        _: &ResourceId,
        _: CreateRequest,
        attempt: CreateAttempt,
    ) -> BoxFuture<'_, Result<CreateOutcome, CreateFailure>> {
        let value = match self.0.lock().unwrap().take().unwrap() {
            Payload::Create(value) => value,
            Payload::Partial(diagnostic) => Ok(attempt
                .issued(
                    AppliedIdentifier::new("fixture").unwrap(),
                    Sent::AllRequestAttributes,
                )
                .partial(Default::default(), diagnostic)),
            _ => panic!("create fixture"),
        };
        Box::pin(async { value })
    }
    fn resume_create(
        &self,
        _: ResumeCreateRequest,
    ) -> BoxFuture<'_, Result<ResumeOutcome, ResumeFailure>> {
        let Some(Payload::Resume(value)) = self.0.lock().unwrap().take() else {
            panic!("resume fixture")
        };
        Box::pin(async { value })
    }
    fn update(
        &self,
        _: &ResourceId,
        _: &AppliedIdentifier,
        _: UpdateRequest,
    ) -> BoxFuture<'_, ProviderResult<UpdateOutcome>> {
        let Some(Payload::Update(value)) = self.0.lock().unwrap().take() else {
            panic!("update fixture")
        };
        Box::pin(async { Ok(value) })
    }
    fn delete(
        &self,
        _: &ResourceId,
        _: &AppliedIdentifier,
        _: DeleteRequest,
    ) -> BoxFuture<'_, ProviderResult<()>> {
        unreachable!()
    }
}
fn fixture(payload: Payload, context: &Context) -> InitializedProvider {
    let keys = match &payload {
        Payload::Partial(diagnostic) => diagnostic.missing_attributes().to_vec(),
        Payload::Create(Ok(outcome)) => outcome
            .diagnostic()
            .map(|d| d.missing_attributes().to_vec())
            .unwrap_or_default(),
        Payload::Create(Err(failure)) => match failure.kind() {
            CreateFailureKind::IdentifierReported { identified, .. } => identified
                .partial_read_diagnostic()
                .map(|d| d.missing_attributes().to_vec())
                .unwrap_or_default(),
            _ => Vec::new(),
        },
        Payload::Update(outcome) => outcome
            .diagnostic()
            .map(|d| d.missing_attributes().to_vec())
            .unwrap_or_default(),
        _ => Vec::new(),
    };
    let schema = keys
        .iter()
        .fold(
            carina_core::schema::ResourceSchema::new("resource"),
            |schema, key| {
                schema.attribute(
                    carina_core::schema::AttributeSchema::optional(
                        key.as_str(),
                        carina_core::schema::AttributeType::string(),
                    )
                    .build(),
                )
            },
        )
        .validate()
        .unwrap();
    block_on(ProviderBoundary::initialize(
        &NativeFactory {
            scope: ReferenceScope::Unavailable,
            normalizer: Mutex::new(None),
            provider: Mutex::new(Some(Box::new(Fixture(Mutex::new(Some(payload)))))),
            schemas: vec![schema],
            name: "text-fixture".into(),
        },
        context.session(),
    ))
    .unwrap()
}
fn id() -> ResourceId {
    Resource::<carina_core::resource::ModuleSource>::with_provider(
        "text-fixture",
        "resource",
        "fixture",
        None,
    )
    .id
}
fn create_request() -> CreateRequest {
    block_on(
        carina_core::validation::deferred_types::with_session_for_test_async(
            &carina_core::parser::LoadedProviderContext::empty_for_test(),
            async |checks| {
                let bindings = Default::default();
                let gate = carina_core::executor::ModuleConstraintGate::from_live_compositions(&[]);
                let schemas = Default::default();
                let context = carina_core::executor::ProviderPreparationContext::new(
                    &bindings,
                    &gate,
                    &[],
                    &NoopNormalizer,
                    &[],
                    &schemas,
                    checks,
                );
                carina_core::executor::provider_ready::prepare_create_request(
                    Resource::with_provider("text-fixture", "resource", "fixture", None),
                    &context,
                )
                .await
                .unwrap()
            },
        ),
    )
}
fn create(payload: Payload, context: &Context) -> Result<HostCreateOutcome, HostCreateFailure> {
    let fixture = fixture(payload, context);
    block_on(
        fixture
            .provider()
            .create(&id(), create_request(), CreateAttempt::for_test(id())),
    )
}
pub trait BindProviderText: Sized {
    type Host;
    fn with_secrets_for_test(self, context: &Context) -> Self::Host;
    fn into_host_for_test(self) -> Self::Host {
        self.with_secrets_for_test(&context(&[]))
    }
}
impl BindProviderText for ProviderError {
    type Host = HostError;
    fn with_secrets_for_test(self, context: &Context) -> HostError {
        let fixture = fixture(Payload::Error(self), context);
        block_on(fixture.provider().read(
            &id(),
            &AppliedIdentifier::new("fixture").unwrap(),
            ReadRequest,
        ))
        .unwrap_err()
    }
}
impl BindProviderText for ProviderDiagnostic {
    type Host = HostDiagnostic;
    fn with_secrets_for_test(self, context: &Context) -> HostDiagnostic {
        ProviderError::from_diagnostic(ProviderErrorKind::Internal, "fixture", self)
            .with_secrets_for_test(context)
            .detail()
            .cause()
            .unwrap()
            .clone()
    }
}
impl BindProviderText for CreateOutcome {
    type Host = HostCreateOutcome;
    fn with_secrets_for_test(self, context: &Context) -> Self::Host {
        create(Payload::Create(Ok(self)), context).unwrap()
    }
}
impl BindProviderText for CreateFailure {
    type Host = HostCreateFailure;
    fn with_secrets_for_test(self, context: &Context) -> Self::Host {
        create(Payload::Create(Err(self)), context).unwrap_err()
    }
}
impl BindProviderText for PartialReadDiagnostic {
    type Host = HostPartialReadDiagnostic;
    fn with_secrets_for_test(self, context: &Context) -> Self::Host {
        create(Payload::Partial(self), context)
            .unwrap()
            .diagnostic()
            .unwrap()
            .clone()
    }
}
impl BindProviderText for IdentifiedCreate {
    type Host = HostIdentifiedCreate;
    fn with_secrets_for_test(self, context: &Context) -> Self::Host {
        let HostCreateFailureKind::IdentifierReported { identified, .. } = self
            .failure_for_test()
            .with_secrets_for_test(context)
            .into_kind()
        else {
            unreachable!()
        };
        identified
    }
}
impl BindProviderText for ResumeFailure {
    type Host = HostResumeFailure;
    fn with_secrets_for_test(self, context: &Context) -> Self::Host {
        resume(Payload::Resume(Err(self)), context).unwrap_err()
    }
}
impl BindProviderText for ResumeOutcome {
    type Host = HostResumeOutcome;
    fn with_secrets_for_test(self, context: &Context) -> Self::Host {
        resume(Payload::Resume(Ok(self)), context).unwrap()
    }
}
fn resume(payload: Payload, context: &Context) -> Result<HostResumeOutcome, HostResumeFailure> {
    let pending = CreateAttempt::for_test(id()).pending(
        PendingToken::new("fixture"),
        Sent::AllRequestAttributes,
        0,
    );
    let request = ResumeCreateRequest::for_test(
        pending.persist(Default::default(), Default::default()),
        create_request(),
    )
    .unwrap();
    block_on(fixture(payload, context).provider().resume_create(request))
}
impl BindProviderText for UpdateOutcome {
    type Host = HostUpdateOutcome;
    fn with_secrets_for_test(self, context: &Context) -> Self::Host {
        // Fixture requests have no changed fields; the scripted provider owns its result.
        block_on(
            carina_core::validation::deferred_types::with_session_for_test_async(
                &carina_core::parser::LoadedProviderContext::empty_for_test(),
                async |checks| {
                    let bindings = Default::default();
                    let gate =
                        carina_core::executor::ModuleConstraintGate::from_live_compositions(&[]);
                    let mut schemas = carina_core::schema::SchemaRegistry::new();
                    schemas.insert(
                        "text-fixture",
                        carina_core::schema::ResourceSchema::new("resource")
                            .validate()
                            .unwrap(),
                    );
                    let request_context = carina_core::executor::ProviderPreparationContext::new(
                        &bindings,
                        &gate,
                        &[],
                        &NoopNormalizer,
                        &[],
                        &schemas,
                        checks,
                    );
                    let request = carina_core::executor::provider_ready::prepare_update_request(
                        Resource::with_provider("text-fixture", "resource", "fixture", None),
                        State::<carina_core::resource::NoPlaceholder>::not_found(id())
                            .to_snapshot(),
                        &[],
                        &request_context,
                    )
                    .await
                    .unwrap();
                    fixture(Payload::Update(self), context)
                        .provider()
                        .update(&id(), &AppliedIdentifier::new("fixture").unwrap(), request)
                        .await
                        .unwrap()
                },
            ),
        )
    }
}
pub trait InspectProviderCause {
    fn redact(&self, context: &Context) -> String;
}
impl InspectProviderCause for ProviderText {
    fn redact(&self, context: &Context) -> String {
        ProviderError::from_text(ProviderErrorKind::Internal, self.clone())
            .with_secrets_for_test(context)
            .to_string()
    }
}
pub trait InspectDiagnostic {
    fn error(&self) -> ProviderCause<'_>;
    fn normalization_for_test(&self) -> Option<RedactedNormalizationCause<'_>>;
}
impl InspectDiagnostic for HostDiagnostic {
    fn error(&self) -> ProviderCause<'_> {
        self.as_cause()
    }
    fn normalization_for_test(&self) -> Option<RedactedNormalizationCause<'_>> {
        self.normalization()
    }
}

pub trait NativeWireFixture {
    fn to_wire(&self) -> carina_core::provider::WireProviderError;
}
impl NativeWireFixture for ProviderError {
    fn to_wire(&self) -> carina_core::provider::WireProviderError {
        self.clone().into_host_for_test().to_wire()
    }
}
pub fn boundary(provider: Box<dyn Provider>) -> ProviderBoundary<'static> {
    initialized(provider).into_boundary()
}
pub fn normalizer(owner: &str, ext: Box<dyn ProviderNormalizer>) -> ConfiguredNormalizer {
    normalizer_with_context(owner, ext, &context(&[]))
}
pub fn normalizer_with_context(
    owner: &str,
    ext: Box<dyn ProviderNormalizer>,
    context: &Context,
) -> ConfiguredNormalizer {
    let mut desired = context.desired.clone();
    desired.id.provider = owner.to_owned();
    let session = fixture_session(owner, None, None, &[desired], &[]);
    block_on(ProviderBoundary::initialize_normalizer(
        &NativeFactory {
            scope: ReferenceScope::Unavailable,
            provider: Mutex::new(None),
            normalizer: Mutex::new(Some(ext)),
            schemas: Vec::new(),
            name: owner.to_owned(),
        },
        &session,
    ))
    .unwrap()
}

pub fn initialized_for_desired<S, D>(
    provider: Box<dyn Provider>,
    desired: &[Resource<
        S,
        carina_core::resource::NoPlaceholder,
        carina_core::resource::AbsoluteBindingAddress,
        D,
    >],
) -> InitializedProvider {
    let name = provider.name().to_owned();
    let session = fixture_session(&name, None, None, desired, &[]);
    let schemas = desired
        .iter()
        .map(|resource| {
            resource
                .attributes
                .keys()
                .fold(
                    carina_core::schema::ResourceSchema::new(&resource.id.resource_type),
                    |schema, key| {
                        schema.attribute(
                            carina_core::schema::AttributeSchema::optional(
                                key,
                                carina_core::schema::AttributeType::string(),
                            )
                            .build(),
                        )
                    },
                )
                .validate()
                .unwrap()
        })
        .collect();
    block_on(ProviderBoundary::initialize(
        &NativeFactory {
            scope: ReferenceScope::Unavailable,
            normalizer: Mutex::new(None),
            provider: Mutex::new(Some(provider)),
            schemas,
            name,
        },
        &session,
    ))
    .unwrap()
}

pub fn idle_provider() -> Box<dyn Provider> {
    Box::new(Fixture(Mutex::new(None)))
}

pub trait InitializeFixture: ProviderFactory + Sized {
    fn initialize_fixture(
        &self,
        binding: Option<&str>,
        config: &ProviderReadyConfig,
    ) -> BoxFuture<'_, HostResult<InitializedProvider>> {
        let session = ProviderPlan::state_only().session(self.name(), binding, Some(config));
        Box::pin(async move { ProviderBoundary::initialize(self, &session).await })
    }
    fn initialize_normalizer_fixture(
        &self,
        binding: Option<&str>,
        config: &ProviderReadyConfig,
    ) -> BoxFuture<'_, HostResult<ConfiguredNormalizer>> {
        let session = ProviderPlan::state_only().session(self.name(), binding, Some(config));
        Box::pin(async move { ProviderBoundary::initialize_normalizer(self, &session).await })
    }
}
impl<T: ProviderFactory> InitializeFixture for T {}

/// Explicitly opaque label for legacy unit probes; grants no Display implementation.
pub trait OpaqueFixtureLabel {
    fn to_string(&self) -> String {
        "<provider text>".into()
    }
}
impl OpaqueFixtureLabel for ProviderError {}
impl OpaqueFixtureLabel for ProviderDiagnostic {}

/// Fixture factories must supply the same schema and normalizer as their provider.
pub fn boundary_with_components(
    provider: Box<dyn Provider>,
    normalizer: Box<dyn ProviderNormalizer>,
    schemas: &carina_core::schema::SchemaRegistry,
) -> ProviderBoundary<'static> {
    let name = schemas.iter().next().map_or_else(
        || provider.name().to_owned(),
        |(owner, _, _, _)| owner.to_owned(),
    );
    let session = ProviderPlan::state_only().session(&name, None, None);
    let factory = NativeFactory {
        scope: ReferenceScope::Unavailable,
        provider: Mutex::new(Some(provider)),
        normalizer: Mutex::new(Some(normalizer)),
        schemas: schemas
            .iter()
            .filter(|(owner, _, _, _)| *owner == name)
            .map(|(_, _, _, schema)| schema.clone())
            .collect(),
        name,
    };
    block_on(ProviderBoundary::initialize(&factory, &session))
        .unwrap()
        .into_boundary()
}

/// Scope fixtures still enter through core's factory mint and fixed session.
pub fn initialized_with_scope(
    provider: Box<dyn Provider>,
    scope: ReferenceScope,
) -> InitializedProvider {
    let context = context(&[]);
    let factory = NativeFactory {
        scope,
        provider: Mutex::new(Some(provider)),
        normalizer: Mutex::new(None),
        schemas: Vec::new(),
        name: "text-fixture".into(),
    };
    block_on(ProviderBoundary::initialize(&factory, context.session())).unwrap()
}
