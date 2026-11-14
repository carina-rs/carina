//! Each fixture owns its user directories, builtin store and command runtime.
use crate::cursor::CursorState;
use crate::output_style::ColorMode;
use crate::runtime::{BuiltinMockInputs, CommandInputs, CommandRuntime, InvocationHooks};
use crate::wiring::TrustedLocalGrantInputs;
use carina_core::{EnvironmentSnapshot, TestUserDirs};
use carina_provider_mock::{ConfiguredBuiltinMock, MockBehavior, MockScope};
use std::sync::{Arc, OnceLock};

/// Obtain a real S3 readiness capability over an in-process HTTP service.
/// The returned request log makes bucket creation and public-access blocking
/// observable without granting a test double its own provisioning method.
#[cfg(test)]
pub(crate) async fn missing_s3_backend(
    auto_create: bool,
) -> (
    Box<dyn carina_state::StateBackend>,
    Arc<std::sync::Mutex<Vec<winterbaume_core::MockRequest>>>,
) {
    use winterbaume_core::{MockAws, MockRequest, MockResponse, MockService};

    struct MissingBucketService {
        requests: Arc<std::sync::Mutex<Vec<MockRequest>>>,
    }

    impl MockService for MissingBucketService {
        fn service_name(&self) -> &str {
            "s3"
        }

        fn url_patterns(&self) -> Vec<&str> {
            vec!["s3.", "s3-"]
        }

        fn handle(
            &self,
            request: MockRequest,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = MockResponse> + Send + '_>>
        {
            Box::pin(async move {
                let status = if request.method == "HEAD" { 404 } else { 200 };
                self.requests.lock().unwrap().push(request);
                MockResponse::xml(status, "")
            })
        }
    }

    let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mock = MockAws::builder()
        .with_service(MissingBucketService {
            requests: Arc::clone(&requests),
        })
        .build();
    let sdk_config = mock.sdk_config("us-east-1").await;
    let raw = parsed_fixture(&format!(
        "backend s3 {{ bucket = \"state-bucket\" key = \"state.json\" auto_create = {auto_create} }}"
    ));
    let config = carina_state::BackendConfig::try_from(raw.backend.as_ref().unwrap()).unwrap();
    let backend = carina_state::s3_backend_for_test(
        aws_sdk_s3::Client::new(&sdk_config),
        config.as_s3().unwrap(),
        "us-east-1".to_owned(),
    );
    (backend, requests)
}

/// Presentation fixtures often omit provider schemas. Give those resource
/// types an explicit coexistence policy; keep any schema supplied by the test.
#[cfg(test)]
pub(crate) fn admit_fixture_plan<S: Clone>(
    plan: &carina_core::plan::Plan<S>,
    schemas: Option<&carina_core::schema::SchemaRegistry>,
) -> carina_core::plan::AdmittedPlan<S> {
    let mut schemas = schemas.cloned().unwrap_or_default();
    for effect in plan.effects() {
        let id = effect.resource_id();
        if schemas
            .get(
                &id.provider,
                &id.resource_type,
                carina_core::schema::SchemaKind::Resource,
            )
            .is_none()
        {
            schemas.insert(
                &id.provider,
                carina_core::schema::PatternResourceSchema::new(&id.resource_type)
                    .with_coexisting_replacement()
                    .validate()
                    .unwrap(),
            );
        }
    }
    plan.clone()
        .admit(&schemas)
        .expect("fixture replacement admission")
}

/// Fixtures that cross analysis boundaries must originate in real syntax.
#[cfg(test)]
pub(crate) fn parsed_fixture(source: &str) -> carina_core::parser::ParsedFile {
    carina_core::parser::parse_file(
        source,
        std::path::Path::new("test.crn"),
        &carina_core::parser::ProviderContext::new(EnvironmentSnapshot::empty()),
    )
    .expect("parse authored fixture")
}

#[cfg(test)]
pub(crate) fn inferred_fixture(source: &str) -> carina_core::parser::InferredFile {
    carina_core::validation::inference::apply_inference(
        parsed_fixture(source),
        &carina_core::schema::SchemaRegistry::new(),
        &carina_core::parser::ProviderContext::new(EnvironmentSnapshot::empty()),
    )
}

#[cfg(test)]
pub(crate) fn composition_declaration_fixture() -> carina_core::parser::DeclarationProvenance {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(directory.path().join("child")).unwrap();
    std::fs::write(
        directory.path().join("child/main.crn"),
        "attributes { value: String = 'initial' }\n",
    )
    .unwrap();
    std::fs::write(
        directory.path().join("main.crn"),
        "let child = use { source = './child' }\nlet checked = child {}\n",
    )
    .unwrap();
    let context = carina_core::parser::ProviderContext::new(EnvironmentSnapshot::empty());
    let schemas = carina_core::schema::SchemaRegistry::new();
    let project = carina_core::paths::ResolvedPath::new(directory.path()).unwrap();
    let loaded =
        carina_core::config_loader::load_configuration_with_config(&project, &context, &schemas)
            .unwrap();
    carina_core::validation::deferred_types::with_session_for_test(&context, |session| {
        carina_core::module_resolver::resolve_modules_with_providerless_config(
            loaded.parsed,
            &project,
            session,
            &schemas,
        )
        .unwrap()
        .compositions
        .remove(0)
        .declaration
    })
}

/// Builds one immutable invocation from the fixture's mock configuration.
/// Reconfiguration starts a new invocation; existing runtime clones stay valid.
/// Installed hooks belong to the fixture and survive invocation rebuilds.
pub struct TestCommandRuntime {
    environment: EnvironmentSnapshot,
    dirs: Arc<TestUserDirs>,
    behavior: MockBehavior,
    scope: MockScope,
    color_mode: ColorMode,
    hooks: InvocationHooks,
    runtime: OnceLock<CommandRuntime>,
}
impl TestCommandRuntime {
    pub fn new() -> Self {
        Self::with_mock(MockBehavior::default(), MockScope::memory())
    }
    pub fn set_mock(&mut self, behavior: MockBehavior, scope: MockScope) {
        self.update_mock(|current_behavior, current_scope| {
            *current_behavior = behavior;
            *current_scope = scope;
        });
    }
    pub(crate) fn update_mock(&mut self, update: impl FnOnce(&mut MockBehavior, &mut MockScope)) {
        update(&mut self.behavior, &mut self.scope);
        self.runtime.take();
    }
    pub fn with_mock(behavior: MockBehavior, scope: MockScope) -> Self {
        Self {
            environment: EnvironmentSnapshot::empty(),
            dirs: Arc::new(TestUserDirs::new()),
            behavior,
            scope,
            color_mode: ColorMode::Plain,
            hooks: InvocationHooks::default(),
            runtime: OnceLock::new(),
        }
    }
    pub fn with_color_mode(mut self, color_mode: ColorMode) -> Self {
        self.color_mode = color_mode;
        self.runtime.take();
        self
    }
    pub fn with_environment(mut self, environment: EnvironmentSnapshot) -> Self {
        self.environment = environment;
        self.runtime.take();
        self
    }
    fn build(&self) -> CommandRuntime {
        let configured = ConfiguredBuiltinMock::new(self.behavior.clone(), self.scope.clone());
        let mut runtime = CommandRuntime::new(
            (**self.dirs).clone(),
            CommandInputs {
                environment: self.environment.clone(),
                builtin_mock: BuiltinMockInputs::deferred(move || Ok(configured)),
                trusted_local_grants: TrustedLocalGrantInputs::new(Vec::new(), None),
                accepted_egress: Default::default(),
                color_mode: self.color_mode,
                cursor: CursorState::new(),
                hooks: self.hooks.clone(),
            },
        );
        runtime.directory_owner = Some(self.dirs.clone());
        runtime
    }
}
impl Default for TestCommandRuntime {
    fn default() -> Self {
        Self::new()
    }
}
impl std::ops::Deref for TestCommandRuntime {
    type Target = CommandRuntime;
    fn deref(&self) -> &Self::Target {
        self.runtime.get_or_init(|| self.build())
    }
}

/// Exercise the real CLI progress renderer from integration tests without
/// exposing the private observer or duplicating its formatting.
pub async fn execute_live_plan_with_cli_observer_for_test(
    provider: &dyn carina_core::provider::HostProvider,
    input: carina_core::executor::LiveExecutionInput<'_>,
    project: &std::path::Path,
) -> carina_core::executor::ExecutionOutcome {
    let observer =
        crate::commands::shared::observer::CliObserver::new(ColorMode::Plain, input.plan, project);
    carina_core::executor::execute_live_plan_for_test(
        provider,
        input,
        &observer,
        carina_core::shutdown::ShutdownToken::running(),
    )
    .await
}

/// Run the real plan/refresh pipeline with native fixture providers.
pub async fn run_refreshing_plan_with_factories(
    runtime: &CommandRuntime,
    path: &std::path::Path,
    factories: Vec<crate::wiring::LoadedProviderFactory>,
) -> Result<bool, crate::error::AppError> {
    crate::commands::plan::run_plan_using(
        runtime,
        path,
        None,
        None,
        None,
        crate::DetailLevel::None,
        false,
        true,
        false,
        false,
        false,
        &carina_core::parser::ProviderContext::new(runtime.environment().clone()),
        |runtime, providers, _| {
            Ok(crate::wiring::ProviderLoad::for_test_with_provider_phase(
                runtime, factories, providers,
            ))
        },
    )
    .await
}

/// Run the full plan command with injected factories and `--refresh=false`.
pub async fn run_plan_with_factories(
    runtime: &CommandRuntime,
    path: &std::path::Path,
    factories: Vec<crate::wiring::LoadedProviderFactory>,
) -> Result<bool, crate::error::AppError> {
    crate::commands::plan::run_plan_using(
        runtime,
        path,
        None,
        None,
        None,
        crate::DetailLevel::None,
        false,
        false,
        false,
        false,
        false,
        &carina_core::parser::ProviderContext::new(runtime.environment().clone()),
        |runtime, providers, _| {
            Ok(crate::wiring::ProviderLoad::for_test_with_provider_phase(
                runtime, factories, providers,
            ))
        },
    )
    .await
}
pub async fn run_detailed_plan_with_factories(
    runtime: &CommandRuntime,
    path: &std::path::Path,
    factories: Vec<crate::wiring::LoadedProviderFactory>,
) -> Result<bool, crate::error::AppError> {
    crate::commands::plan::run_plan_using(
        runtime,
        path,
        None,
        None,
        None,
        crate::DetailLevel::Full,
        false,
        false,
        false,
        false,
        false,
        &carina_core::parser::ProviderContext::new(runtime.environment().clone()),
        |runtime, providers, _| {
            Ok(crate::wiring::ProviderLoad::for_test_with_provider_phase(
                runtime, factories, providers,
            ))
        },
    )
    .await
}

#[cfg(test)]
pub(crate) use graph_fixtures::{compiled_loop, compiled_replace, persisted, resolved};

#[cfg(test)]
mod graph_fixtures {
    use carina_core::{
        effect::{DeferredCreatePayload, DeferredReplacePayload, NonEmptyDeletes},
        parser::{ExpandedDeferredForExpression, ExpandedParsedFile},
        resource::{PersistedDestroyDependencies, ResolvedResourceId, Resource},
    };
    pub fn compiled_loop(template: Box<ExpandedDeferredForExpression>) -> DeferredCreatePayload {
        let mut configuration = ExpandedParsedFile::default();
        configuration.upstream_states = ["orgs", "upstream"]
            .into_iter()
            .map(|binding| carina_core::parser::UpstreamState {
                binding: crate::absolute_binding_for_test(binding),
                source: "upstream".into(),
            })
            .collect();
        configuration.resources = ["cert", "parent", "primary", "fallback"]
            .into_iter()
            .map(|binding| {
                Resource::with_provider("mock", "test.resource", binding, None)
                    .with_absolute_binding(crate::absolute_binding_for_test(binding))
            })
            .collect();
        let graph = carina_core::resolved_graph::resolve_expanded_graph(configuration.into())
            .expect("fixture graph");
        let (template, dependency_template) = graph
            .compile_deferred_for(*template)
            .expect("fixture loop dependencies");
        DeferredCreatePayload::new(Box::new(template), dependency_template)
    }
    pub fn compiled_replace(
        deletes: NonEmptyDeletes,
        template: Box<ExpandedDeferredForExpression>,
    ) -> DeferredReplacePayload {
        let payload = compiled_loop(template);
        DeferredReplacePayload::new(deletes, payload.template, payload.dependency_template)
    }
    pub fn persisted(
        rows: impl IntoIterator<Item = ResolvedResourceId>,
    ) -> PersistedDestroyDependencies {
        PersistedDestroyDependencies::from_test_dependencies(rows)
    }
    pub fn resolved(resource: Resource) -> carina_core::resource::ResolvedResource {
        carina_core::resource::ResolvedResource::collect_resolved([resource])
            .expect("fixture graph resolves")
            .remove(0)
    }
}

#[cfg(test)]
mod removed_refresh;

#[cfg(test)]
pub(crate) fn refresh_current(
    current: carina_core::resource::Current,
) -> carina_state::RefreshableCurrent {
    let id = current.id().clone();
    carina_state::PreparedRows::from_snapshots(
        [(id.clone(), current)],
        &carina_state::ResolvedRemovedClaims::resolve(&[], None),
    )
    .refreshable
    .current(&id)
    .unwrap()
}

#[cfg(test)]
pub(crate) fn refresh_target(
    id: &carina_core::resource::ResourceId,
    identifier: &carina_core::provider::AppliedIdentifier,
) -> carina_state::RefreshTarget {
    refresh_current(carina_core::resource::Current::from_state(
        carina_core::resource::State::<carina_core::resource::NoPlaceholder>::existing(
            id.clone(),
            std::collections::HashMap::new(),
        )
        .with_identifier(identifier.as_str()),
    ))
    .read_target()
    .unwrap()
}

#[cfg(test)]
pub(crate) fn empty_state_block_targets() -> crate::wiring::ResolvedStateBlockTargets {
    crate::wiring::ResolvedStateBlockTargets {
        removed_from: carina_state::ResolvedRemovedClaims::resolve(&[], None),
    }
}

pub use crate::provider_text_fixtures::{boundary, normalizer};
#[cfg(test)]
#[path = "../../carina-core/tests/support/creation.rs"]
pub(crate) mod creation_fixture;

/// A test-only declaration whose values are consumed by the configuration harness.
#[cfg(test)]
pub(crate) struct ConfigurableMockFactory {
    config: carina_core::test_utils::configuration::ConfigFactory,
    provider: ConfiguredBuiltinMock,
}
#[cfg(test)]
impl ConfigurableMockFactory {
    pub(crate) fn with_string_setting(key: &str) -> Self {
        Self {
            config: carina_core::test_utils::configuration::ConfigFactory::new(
                carina_core::schema::ValidatedConfigSchema::validate(
                    std::collections::BTreeMap::from([(
                        key.into(),
                        carina_core::schema::AttributeType::string(),
                    )]),
                )
                .unwrap(),
            )
            .named("mock"),
            provider: ConfiguredBuiltinMock::new(Default::default(), MockScope::memory()),
        }
    }
}
#[cfg(test)]
impl carina_core::provider::ProviderFactory for ConfigurableMockFactory {
    fn name(&self) -> &str {
        "mock"
    }
    fn display_name(&self) -> &str {
        "configuration test provider"
    }
    fn validate_config(
        &self,
        attrs: &carina_core::provider::CanonicalProviderConfigAttributes,
        inputs: &carina_core::provider::ProviderValidationInputs,
    ) -> carina_core::provider::ProviderResult<()> {
        self.config.validate_config(attrs, inputs)
    }
    fn schemas(&self) -> Vec<carina_core::schema::ValidatedResourceSchema> {
        carina_provider_mock::builtin_schemas()
    }
    fn schema_witness(&self) -> carina_core::schema::ValidatedProviderSchemaWitness {
        carina_core::schema::ValidatedProviderSchemaWitness::new(
            self.schemas(),
            self.config.schema.clone(),
            Default::default(),
            Vec::new(),
        )
        .unwrap()
    }
    fn create_provider(
        &self,
        session: &carina_core::provider::ProviderSession,
        _: carina_core::provider::InitializeOutcomeMint,
    ) -> carina_core::provider::BoxFuture<
        '_,
        carina_core::provider::ProviderResult<carina_core::provider::ProviderInitialization>,
    > {
        assert_eq!(
            self.config.seen.lock().unwrap().last(),
            Some(session.configuration().attributes())
        );
        let provider = self.provider.create_provider();
        Box::pin(async {
            Ok(carina_core::provider::InitializeOutcome::unavailable()
                .with_provider(Box::new(provider)))
        })
    }
}

#[cfg(test)]
pub(crate) fn install_mock_wasm(directory: &std::path::Path) {
    let workspace = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap();
    let source = ["carina_provider_mock.wasm", "carina-provider-mock.wasm"]
        .into_iter()
        .map(|name| workspace.join("target/wasm32-wasip2/debug").join(name))
        .find(|path| path.is_file())
        .expect("the mock WASM fixture must be built");
    std::fs::copy(&source, directory.join("mock.wasm")).unwrap();
    let installed = directory.join(".carina/providers/file/mock/mock.wasm");
    std::fs::create_dir_all(installed.parent().unwrap()).unwrap();
    std::fs::copy(source, installed).unwrap();
}
