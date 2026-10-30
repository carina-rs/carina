use super::*;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use carina_core::data_source_value::{DataSourceValue, DataSourceValues};
use carina_core::effect::PlanOp;
use carina_core::executor::{
    ExecutionEvent, ExecutionObserver, ExecutionOutcome, LiveExecutionInput, ProviderCheckInputs,
    UnresolvedResource, execute_live_plan,
};
use carina_core::provider::{
    AppliedIdentifier, AppliedState, BoxFuture, CreateOutcome, CreateRequest, DeleteRequest,
    ProviderResult, ReadOutcome, ReadRequest, Sent, UpdateOutcome, UpdateRequest,
};
use carina_core::resource::{
    Directives, ResolvedResource, ResolvedResourceId, Resource, ResourceId, ResourceIdentity, Value,
};
use carina_core::schema::{StructField, TypeIdentity};
use carina_core::value::canonicalize_resources_with_schemas;

#[test]
fn saved_plan_validation_ready_constructor_is_unit_typed() {
    let ready: ValidationReady<()> = ValidationReady::from_saved_plan_artifact();
    let ValidationReady { inner: (), origin } = ready;

    assert!(matches!(origin, ValidationReadyOrigin::SavedPlanArtifact));
}

fn missing_identifier_plan_error(identity: &str) -> carina_core::plan::PlanError {
    carina_core::plan::PlanError::new(
        ResourceId::with_identity("test.thing", test_identity(identity)),
        carina_core::plan::PlanErrorKind::MissingProviderIdentifier,
    )
}

#[test]
fn non_empty_plan_errors_reject_empty_and_preserve_order() {
    assert!(NonEmptyPlanErrors::try_from(Vec::new()).is_err());

    let first = missing_identifier_plan_error("first");
    let second = missing_identifier_plan_error("second");
    let errors = NonEmptyPlanErrors::try_from(vec![first.clone(), second.clone()])
        .expect("two errors are non-empty");

    assert_eq!(errors.as_slice(), &[first, second]);
}

#[tokio::test]
async fn post_expansion_seam_carries_non_empty_preparation_diagnostics() {
    let context = WiringContext::new_for_test(Vec::new(), &[]);
    let expanded_resources = ExpandedCanonicalResources {
        resources: canonical_unresolved(Vec::new(), context.schemas()),
    };
    let expansion = PostExpansionPlanView {
        data: PostExpansionPlanData {
            expanded_resources,
            sorted_resources: Vec::new(),
            addressed: AddressedState {
                managed_current: HashMap::new(),
                orphans: Orphans::new(),
                saved_attrs: crate::empty_lifted_saved_attrs(),
                prev_explicit: HashMap::new(),
                moved_pairs: Vec::new(),
            },
            data_source_values: DataSourceValues::empty(),
            wait_aliases: Vec::new(),
            residual_deferred_for: Vec::new(),
            deferred_create_targets: Vec::new(),
        },
        children: AbsentChildrenSettled,
    }
    .into_plan_settled();
    let provider = carina_core::provider::ProviderRouter::new();
    let state_file = None;
    let state_block_claims = StateBlockClaims::default();
    let resolved_state_block_targets = ResolvedStateBlockTargets::default();
    let expected = missing_identifier_plan_error("diagnostic");
    let preparation_errors =
        NonEmptyPlanErrors::try_from(vec![expected.clone()]).expect("one error is non-empty");

    let outcome = build_post_expansion_plan::<PlanMode, _>(PostExpansionPlanInput {
        context: &context,
        expansion,
        environment: PostExpansionPlanEnvironment {
            data_sources: &[],
            provider: &provider,
            normalizer: &provider,
            provider_configs: &[],
            remote_bindings: &HashMap::new(),
            state_file: &state_file,
            state_block_claims: &state_block_claims,
            resolved_state_block_targets: &resolved_state_block_targets,
            preparation_errors: Some(preparation_errors),
        },
    })
    .await
    .expect("typed preparation diagnostics should produce an outcome");

    let PostExpansionOutcome::Diagnostics(diagnostics) = outcome else {
        panic!("non-empty preparation diagnostics must not produce a plan")
    };
    let errors = diagnostics.errors().clone();
    let artifacts = diagnostics.into_artifacts();
    assert_eq!(errors.as_slice(), &[expected]);
    assert_eq!(artifacts.plan.errors(), errors.as_slice());
}

struct RawEnumStateFactory;

impl ProviderFactory for RawEnumStateFactory {
    fn name(&self) -> &str {
        "test"
    }

    fn display_name(&self) -> &str {
        "raw enum state test provider"
    }

    fn provider_config_attribute_types(&self) -> carina_core::schema::ValidatedConfigSchema {
        Default::default()
    }

    fn validate_config(
        &self,
        _attributes: &carina_core::provider::CanonicalProviderConfigAttributes,
    ) -> ProviderResult<()> {
        Ok(())
    }

    fn extract_region(&self, _attributes: &IndexMap<String, Value>) -> String {
        String::new()
    }

    fn create_provider(
        &self,
        _binding: Option<&str>,
        _config: &ProviderReadyConfig,
    ) -> BoxFuture<'_, ProviderResult<Box<dyn Provider>>> {
        Box::pin(async { Ok(Box::new(RawEnumStateProvider) as Box<dyn Provider>) })
    }

    fn create_normalizer(
        &self,
        _binding: Option<&str>,
        _config: &ProviderReadyConfig,
    ) -> BoxFuture<'_, ProviderResult<Box<dyn ProviderNormalizer>>> {
        Box::pin(async {
            Ok(Box::new(carina_core::provider::NoopNormalizer) as Box<dyn ProviderNormalizer>)
        })
    }

    fn schemas(&self) -> Vec<carina_core::schema::ValidatedResourceSchema> {
        let mode = AttributeType::enum_(
            carina_core::schema::enum_identity("Mode", Some("test.thing")),
            Some(vec!["Allow".to_string()]),
            vec![("Allow".to_string(), "allow".to_string())],
            None,
            None,
        );
        vec![
            ResourceSchema::new("thing")
                .attribute(AttributeSchema::required("mode", mode).build())
                .validate()
                .expect("raw enum test schema is valid"),
        ]
    }
}

struct RawEnumStateProvider;

impl Provider for RawEnumStateProvider {
    fn name(&self) -> &str {
        "test"
    }

    fn read(
        &self,
        id: &ResourceId,
        identifier: &AppliedIdentifier,
        _request: ReadRequest,
    ) -> BoxFuture<'_, ProviderResult<ReadOutcome>> {
        let id = id.clone();
        let identifier = identifier.clone();
        Box::pin(async move {
            Ok(ReadOutcome::found(
                AppliedState::new(
                    id,
                    identifier.to_string(),
                    HashMap::from([(
                        "mode".to_string(),
                        Value::Concrete(ConcreteValue::String("Allow".into())),
                    )]),
                )
                .expect("raw enum test identifier is non-empty"),
            ))
        })
    }

    fn read_data_source(
        &self,
        _resource: &carina_core::provider::ProviderReadyDataSource,
    ) -> BoxFuture<'_, ProviderResult<State>> {
        Box::pin(async { panic!("raw enum test must not read a data source") })
    }

    fn create(
        &self,
        _id: &ResourceId,
        _request: CreateRequest,
        _attempt: carina_core::provider::CreateAttempt,
    ) -> BoxFuture<'_, Result<CreateOutcome, carina_core::provider::CreateFailure>> {
        Box::pin(async { panic!("raw enum test must not create") })
    }

    fn resume_create(
        &self,
        _request: carina_core::provider::ResumeCreateRequest,
    ) -> BoxFuture<
        '_,
        Result<carina_core::provider::ResumeOutcome, carina_core::provider::ResumeFailure>,
    > {
        Box::pin(async { panic!("raw enum test must not resume create") })
    }

    fn update(
        &self,
        _id: &ResourceId,
        _identifier: &AppliedIdentifier,
        _request: UpdateRequest,
    ) -> BoxFuture<'_, ProviderResult<UpdateOutcome>> {
        Box::pin(async { panic!("raw enum test must not update") })
    }

    fn delete(
        &self,
        _id: &ResourceId,
        _identifier: &AppliedIdentifier,
        _request: DeleteRequest,
    ) -> BoxFuture<'_, ProviderResult<()>> {
        Box::pin(async { panic!("raw enum test must not delete") })
    }

    fn required_permissions(&self, _id: &ResourceId, _op: PlanOp) -> ProviderResult<Vec<String>> {
        Ok(Vec::new())
    }
}

fn raw_enum_state_file() -> StateFile {
    let mut row =
        carina_state::state::ResourceState::try_applied("thing", "item", "test", "provider-id")
            .expect("raw enum test identifier is non-empty")
            .with_attribute("mode", serde_json::json!("Allow"));
    row.binding = Some("item".to_string());
    let mut state = StateFile::new();
    state
        .upsert_resource(row)
        .expect("raw enum test state row is unique");
    state
}

#[tokio::test]
async fn plan_lifts_raw_api_enum_prior_state_on_live_and_cached_paths() {
    let parsed = parse(
        r#"
provider test {}

let item = test.thing {
  mode = test.thing.Mode.allow
}
"#,
        &carina_core::parser::ProviderContext::default(),
    )
    .expect("parse raw enum plan fixture");
    let provider_configs = resolved_provider_configs(&parsed);

    for refresh in [true, false] {
        let wiring = ValidatedWiring::from_factories_for_test(
            vec![Box::new(RawEnumStateFactory)],
            provider_configs.as_provider_configs(),
        );
        let canonical_resources = canonical_pair(
            &parsed,
            parsed.resources.clone(), // allow: direct — plan-time reconciliation
            wiring.context().schemas(),
        );
        let validated = ValidatedConfig::from_wiring_for_test(wiring, canonical_resources);
        let state_file = Some(raw_enum_state_file());
        let planned = create_plan_from_parsed_with_upstream_with_ctx(
            validated,
            &provider_configs,
            &state_file,
            refresh,
            ProgressSink::Human,
            &HashMap::new(),
            &StateBlockClaims::default(),
            &ResolvedStateBlockTargets::default(),
        )
        .await
        .unwrap_or_else(|error| panic!("refresh={refresh} plan failed: {error}"));
        let planned = planned.resources();

        assert!(
            planned.plan.effects().is_empty(),
            "refresh={refresh} produced a phantom enum diff: {:#?}",
            planned.plan.effects(),
        );
        let id = ResourceId::with_provider_identity("test", "thing", "item", None);
        let Current::Present(prior) = &planned.managed_current[&id] else {
            panic!("refresh={refresh} prior state is not present")
        };
        assert!(
            matches!(
                prior.state().attributes.get("mode"),
                Some(Value::Concrete(ConcreteValue::CanonicalEnum(value)))
                    if value.api_value() == "Allow"
            ),
            "refresh={refresh} did not lift raw enum state: {:?}",
            prior.state().attributes.get("mode"),
        );
    }
}

#[test]
fn data_source_reads_select_only_the_owning_identical_anonymous_call() {
    let tmp = tempfile::tempdir().expect("temporary project");
    let module_dir = tmp.path().join("modules/query");
    let root_dir = tmp.path().join("root");
    std::fs::create_dir_all(&module_dir).unwrap();
    std::fs::create_dir_all(&root_dir).unwrap();
    std::fs::write(
        module_dir.join("main.crn"),
        r#"
attributes {
  marker = "fixed"
}

let matches = read mock.test.Matches {
  pattern = "fixed"
}
"#,
    )
    .unwrap();
    let root_body = r#"
let query = use {
  source = "$MODULE"
}

query {}
query {}
"#
    .replace("$MODULE", &module_dir.display().to_string());
    let root_file = root_dir.join("main.crn");
    std::fs::write(&root_file, &root_body).unwrap();

    let config = carina_core::parser::ProviderContext::default();
    let mut parsed = carina_core::parser::parse_file(&root_body, &root_file, &config)
        .expect("root fixture must parse");
    carina_core::module_resolver::resolve_modules_with_providerless_config(
        &mut parsed,
        &root_dir,
        &config,
        &SchemaRegistry::new(),
    )
    .expect("anonymous calls must expand");

    assert_eq!(
        parsed.compositions.len(),
        2,
        "fixture must produce two calls"
    );
    assert_eq!(
        parsed.data_sources.len(),
        2,
        "each anonymous call must produce its own query"
    );
    let relevant = relevant_compositions_for_data_source_reads(
        &[],
        &parsed.compositions,
        std::slice::from_ref(&parsed.data_sources[0]),
        &parsed.data_sources,
    );

    assert_eq!(relevant.len(), 1);
    assert_eq!(
        relevant[0].call_path().immediate().id,
        parsed.compositions[0].call_path().immediate().id,
        "a query must be gated only by the parser-minted call path that owns it"
    );
}

#[test]
fn data_source_reads_include_every_ancestor_module_gate() {
    let tmp = tempfile::tempdir().expect("temporary project");
    let inner_dir = tmp.path().join("modules/inner");
    let outer_dir = tmp.path().join("modules/outer");
    let root_dir = tmp.path().join("root");
    for directory in [&inner_dir, &outer_dir, &root_dir] {
        std::fs::create_dir_all(directory).unwrap();
    }
    std::fs::write(
        inner_dir.join("main.crn"),
        r#"
let matches = read mock.test.Matches {
  pattern = "fixed"
}

attributes {
  marker = "inner"
}
"#,
    )
    .unwrap();
    std::fs::write(
        outer_dir.join("main.crn"),
        format!(
            r#"
let inner = use {{
  source = "{}"
}}

inner {{}}

attributes {{
  marker = "outer"
}}
"#,
            inner_dir.display(),
        ),
    )
    .unwrap();
    let root_body = format!(
        r#"
let outer = use {{
  source = "{}"
}}

outer {{}}
"#,
        outer_dir.display(),
    );
    let root_file = root_dir.join("main.crn");
    std::fs::write(&root_file, &root_body).unwrap();

    let config = carina_core::parser::ProviderContext::default();
    let mut parsed = carina_core::parser::parse_file(&root_body, &root_file, &config)
        .expect("root fixture must parse");
    carina_core::module_resolver::resolve_modules_with_providerless_config(
        &mut parsed,
        &root_dir,
        &config,
        &SchemaRegistry::new(),
    )
    .expect("nested calls must expand");

    let outer = parsed
        .compositions
        .iter()
        .find(|composition| composition.call_path().calls().count() == 1)
        .expect("outer composition");
    let relevant = relevant_compositions_for_data_source_reads(
        &[],
        &parsed.compositions,
        &parsed.data_sources,
        &parsed.data_sources,
    );

    assert!(
        relevant.iter().any(|composition| {
            composition.call_path().immediate().id == outer.call_path().immediate().id
        }),
        "the query's outer ancestor must retain its module gate: {relevant:#?}",
    );
}

#[test]
fn execution_minting_checks_load_completeness_after_skipped_validation_in_provider_name_order() {
    let provider_configs = || {
        ["zeta", "alpha"]
            .into_iter()
            .map(|name| ProviderConfig {
                name: name.to_string(),
                source: Some(
                    format!("file:///definitely-missing/{name}.wasm")
                        .parse()
                        .unwrap(),
                ),
                version: None,
                revision: None,
                unresolved_attributes: IndexMap::new(),
                binding: None,
                attributes: IndexMap::new(),
                default_tags: IndexMap::new(),
            })
            .collect::<Vec<_>>()
    };
    let incomplete_load = || {
        let provider_configs = provider_configs();
        ProviderLoad::for_test_with_load_errors(
            Vec::new(),
            &provider_configs,
            HashMap::from([
                ("zeta".to_string(), "zeta failed".to_string()),
                ("alpha".to_string(), "alpha failed".to_string()),
            ]),
        )
    };
    let invalid_config = || {
        let mut parsed = carina_core::parser::InferredFile::default();
        parsed.providers = provider_configs();
        parsed.arguments = vec![carina_core::parser::ArgumentParameter {
            name: "unrelated".to_string(),
            type_expr: carina_core::parser::TypeExpr::String,
            default: None,
            description: None,
            validations: Vec::new(),
        }];
        parsed
    };

    let evaluated_error = match incomplete_load().validate_evaluated_config(
        invalid_config(),
        Path::new("."),
        true,
        &[],
        &[],
    ) {
        Ok(_) => panic!("incomplete provider loads must not mint execution wiring"),
        Err(error) => error,
    };
    let unresolved = carina_core::parser::ParsedFile::default();
    let pair_error = match incomplete_load().validate_config_pair(
        invalid_config(),
        carina_core::config_loader::UnresolvedConfigSnapshot::from_parsed(unresolved),
        Path::new("."),
        true,
        &[],
        &[],
    ) {
        Ok(_) => panic!("incomplete provider loads must not mint execution wiring"),
        Err(error) => error,
    };

    let expected = "arguments blocks are only valid inside module definitions, not in root configurations.\nalpha failed\nzeta failed";
    assert_eq!(evaluated_error.to_string(), expected);
    assert_eq!(pair_error.to_string(), expected);
}

#[test]
fn plan_style_config_pair_minting_collects_load_and_unrelated_validation_errors() {
    let provider = ProviderConfig {
        name: "broken".to_string(),
        source: Some("file:///definitely-missing/broken.wasm".parse().unwrap()),
        version: None,
        revision: None,
        unresolved_attributes: IndexMap::new(),
        binding: None,
        attributes: IndexMap::new(),
        default_tags: IndexMap::new(),
    };
    let providers = ProviderLoad::for_test_with_load_errors(
        Vec::new(),
        std::slice::from_ref(&provider),
        HashMap::from([("broken".to_string(), "broken provider failed".to_string())]),
    );
    let mut parsed = carina_core::parser::InferredFile::default();
    parsed.providers = vec![provider];
    parsed.arguments = vec![carina_core::parser::ArgumentParameter {
        name: "unrelated".to_string(),
        type_expr: carina_core::parser::TypeExpr::String,
        default: None,
        description: None,
        validations: Vec::new(),
    }];

    let unresolved = carina_core::config_loader::UnresolvedConfigSnapshot::from_parsed(
        carina_core::parser::ParsedFile::default(),
    );
    let error =
        match providers.validate_config_pair(parsed, unresolved, Path::new("."), false, &[], &[]) {
            Ok(_) => panic!("plan-style validation must reject both independent errors"),
            Err(error) => error,
        };
    let message = error.to_string();

    assert!(message.contains("broken provider failed"), "{message}");
    assert!(message.contains("arguments blocks"), "{message}");
}

#[test]
fn hard_reporting_load_returns_schema_only_diagnostics() {
    let user_dirs = carina_core::TestUserDirs::new();
    let base = tempfile::tempdir().unwrap();
    let provider = ProviderConfig {
        name: "mock".to_string(),
        source: Some("file:///definitely-missing/mock.wasm".parse().unwrap()),
        version: None,
        revision: None,
        unresolved_attributes: IndexMap::new(),
        binding: None,
        attributes: IndexMap::new(),
        default_tags: IndexMap::new(),
    };
    let (providers, hard_error) =
        ProviderLoad::load_reporting(&user_dirs, &[provider.clone(), provider], base.path());
    assert!(
        hard_error
            .as_ref()
            .is_some_and(|error| error.to_string().contains("declared more than once")),
        "fixture must exercise the hard loader failure"
    );

    let mut parsed = carina_core::parser::InferredFile::default();
    let report = providers.validation_report(&mut parsed, base.path(), true, &[], &[]);

    assert!(report.errors().is_empty());
    assert!(report.schemas().iter().next().is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn factory_building_parses_grants_lazily_and_threads_matching_trust() {
    let user_dirs = carina_core::TestUserDirs::new();
    let tmp = tempfile::tempdir().unwrap();
    let source_path = tmp.path().join("mock-provider.wasm");
    let installed_path = tmp
        .path()
        .join(".carina/providers/file/mock-provider/mock-provider.wasm");
    std::fs::create_dir_all(installed_path.parent().unwrap()).unwrap();
    std::fs::write(&source_path, b"mock fixture").unwrap();
    std::fs::write(&installed_path, b"mock fixture").unwrap();
    let config = ProviderConfig {
        name: "aws".into(),
        source: Some(format!("file://{}", source_path.display()).parse().unwrap()),
        version: None,
        revision: None,
        unresolved_attributes: IndexMap::new(),
        binding: None,
        attributes: IndexMap::new(),
        default_tags: IndexMap::new(),
    };

    let invalid = TrustedLocalGrantInputs::new(Vec::new(), Some("not-a-grant".into()));
    let loader_called = Arc::new(AtomicBool::new(false));
    let called = Arc::clone(&loader_called);
    let (factories, errors) = build_factories_from_providers_impl_using(
        &user_dirs,
        &[],
        tmp.path(),
        false,
        &invalid,
        move |_verified| {
            called.store(true, Ordering::SeqCst);
            async {
                Err(carina_plugin_host::WasmProviderLoadError::Other(
                    "unexpected load".into(),
                ))
            }
        },
    )
    .unwrap();
    assert!(factories.is_empty());
    assert!(errors.is_empty());
    assert!(invalid.parsed.get().is_none());

    let called = Arc::clone(&loader_called);
    let result = build_factories_from_providers_impl_using(
        &user_dirs,
        std::slice::from_ref(&config),
        tmp.path(),
        false,
        &invalid,
        move |_verified| {
            called.store(true, Ordering::SeqCst);
            async {
                Err(carina_plugin_host::WasmProviderLoadError::Other(
                    "unexpected load".into(),
                ))
            }
        },
    );
    let Err(error) = result else {
        panic!("invalid grants must fail when the first provider load begins")
    };
    assert!(matches!(
        error,
        AppError::TrustedLocalGrant(
            carina_provider_resolver::GrantParseError::MissingSeparator { .. }
        )
    ));
    assert!(!loader_called.load(Ordering::SeqCst));

    let valid = TrustedLocalGrantInputs::new(vec![format!("{}=aws", source_path.display())], None);
    let saw_aws = Arc::new(AtomicBool::new(false));
    let observed = Arc::clone(&saw_aws);
    let (factories, errors) = build_factories_from_providers_impl_using(
        &user_dirs,
        &[config],
        tmp.path(),
        false,
        &valid,
        move |verified| {
            observed.store(
                verified.identity().trust().class()
                    == carina_provider_resolver::ProviderTrustClass::Aws,
                Ordering::SeqCst,
            );
            async {
                Err(carina_plugin_host::WasmProviderLoadError::Other(
                    "stop after trust observation".into(),
                ))
            }
        },
    )
    .unwrap();

    assert!(factories.is_empty());
    assert!(
        errors
            .get("aws")
            .is_some_and(|message| message.contains("stop after trust observation")),
        "the injected loader error should prove the production path was reached: {errors:?}"
    );
    assert!(saw_aws.load(Ordering::SeqCst));
}

#[tokio::test(flavor = "multi_thread")]
async fn duplicate_sourced_default_provider_kind_stops_before_a_second_load() {
    let user_dirs = carina_core::TestUserDirs::new();
    let tmp = tempfile::tempdir().unwrap();
    let source_path = tmp.path().join("duplicate-provider.wasm");
    let installed_path = tmp
        .path()
        .join(".carina/providers/file/duplicate-provider/duplicate-provider.wasm");
    std::fs::create_dir_all(installed_path.parent().unwrap()).unwrap();
    std::fs::write(&source_path, b"duplicate fixture").unwrap();
    std::fs::write(&installed_path, b"duplicate fixture").unwrap();
    let config = ProviderConfig {
        name: "duplicate".into(),
        source: Some(format!("file://{}", source_path.display()).parse().unwrap()),
        version: None,
        revision: None,
        unresolved_attributes: IndexMap::new(),
        binding: None,
        attributes: IndexMap::new(),
        default_tags: IndexMap::new(),
    };
    let configs = [config.clone(), config];
    let inputs = TrustedLocalGrantInputs::new(Vec::new(), None);
    let hard_loads = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&hard_loads);

    let result = build_factories_from_providers_impl_using(
        &user_dirs,
        &configs,
        tmp.path(),
        true,
        &inputs,
        move |_verified| {
            observed.fetch_add(1, Ordering::SeqCst);
            async {
                Err(carina_plugin_host::WasmProviderLoadError::Other(
                    "first load failed".into(),
                ))
            }
        },
    );
    let Err(error) = result else {
        panic!("duplicate sourced defaults must be a hard validation error")
    };

    let AppError::Validation(message) = error else {
        panic!("expected duplicate-provider validation error")
    };
    assert!(message.contains("duplicate"), "{message}");
    assert_eq!(hard_loads.load(Ordering::SeqCst), 1);

    let formatting_loads = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&formatting_loads);
    let (factories, errors) = build_factories_from_providers_impl_using(
        &user_dirs,
        &configs,
        tmp.path(),
        false,
        &inputs,
        move |_verified| {
            observed.fetch_add(1, Ordering::SeqCst);
            async {
                Err(carina_plugin_host::WasmProviderLoadError::Other(
                    "first load failed".into(),
                ))
            }
        },
    )
    .expect("formatting must retain duplicate-provider failure as a diagnostic");

    assert!(factories.is_empty());
    assert_eq!(formatting_loads.load(Ordering::SeqCst), 1);
    assert!(
        errors
            .get("duplicate")
            .is_some_and(|message| message.contains("declared more than once")),
        "formatting must report the duplicate provider kind: {errors:?}"
    );
}

#[test]
fn first_provider_load_surfaces_invalid_trusted_local_inputs() {
    let inputs = TrustedLocalGrantInputs::new(Vec::new(), Some("not-a-grant".into()));

    let error = inputs.get().unwrap_err();

    assert!(matches!(
        error,
        carina_provider_resolver::GrantParseError::MissingSeparator { .. }
    ));
    assert!(inputs.parsed.get().is_none());
}

fn parse(
    input: &str,
    config: &carina_core::parser::ProviderContext,
) -> Result<carina_core::parser::ParsedFile, carina_core::parser::ParseError> {
    carina_core::parser::parse_file(input, std::path::Path::new("wiring_test.crn"), config)
}

fn pathless_test_type(attr_type: AttributeType) -> carina_core::schema::RootlessType {
    carina_core::schema::ValidatedConfigSchema::validate(BTreeMap::from([(
        "value".to_string(),
        attr_type,
    )]))
    .expect("test config type must be valid")
    .rootless_type("value")
    .expect("test config type must exist")
}

macro_rules! unexpected_resume_create {
    () => {
        fn resume_create(
            &self,
            _request: carina_core::provider::ResumeCreateRequest,
        ) -> BoxFuture<
            '_,
            Result<carina_core::provider::ResumeOutcome, carina_core::provider::ResumeFailure>,
        > {
            Box::pin(async { panic!("unexpected resume_create") })
        }
    };
}

enum ReadBehavior {
    Success,
    NotFound,
    ApiError,
    NotFoundThenSuccess,
}

fn resolved(resource: Resource) -> ResolvedResource {
    ResolvedResource::new(resource)
}

fn test_data_source_values(
    data_sources: &[DataSource],
    states: &HashMap<ResourceId, State>,
) -> DataSourceValues {
    DataSourceValues::from_data_sources(data_sources, |data_source| {
        states.get(&data_source.id).map_or_else(
            || DataSourceValue::deferred_to_apply(Vec::new()),
            |state| DataSourceValue::read(state.clone()),
        )
    })
}

fn canonical_unresolved(
    resources: Vec<Resource>,
    schemas: &SchemaRegistry,
) -> CanonicalResourcePair<carina_core::parser::ParsedExportParam> {
    let mut unresolved = carina_core::parser::ParsedFile::default();
    unresolved.resources = resources;
    resolve_config_block_names_for_test(
        unresolved.clone(),
        carina_core::config_loader::UnresolvedConfigSnapshot::from_parsed(unresolved),
        schemas,
    )
    .expect("test block-name resolution must succeed")
}

const TEST_COMMIT_SHA: &str = "abcdef0123456789abcdef0123456789abcdef01";

fn test_commit_sha() -> carina_provider_resolver::CommitSha {
    serde_json::from_str(&format!("{TEST_COMMIT_SHA:?}"))
        .expect("test commit SHA must be canonical")
}

fn test_artifact_id(value: u64) -> carina_provider_resolver::ArtifactId {
    serde_json::from_str(&value.to_string()).expect("test artifact ID must be numeric")
}

fn canonical_pair<E: Clone>(
    evaluated: &carina_core::parser::File<E>,
    unresolved_resources: Vec<Resource>,
    schemas: &SchemaRegistry,
) -> CanonicalResourcePair<E> {
    let mut unresolved = carina_core::parser::ParsedFile::default();
    unresolved.resources = unresolved_resources;
    resolve_config_block_names_for_test(
        evaluated.clone(),
        carina_core::config_loader::UnresolvedConfigSnapshot::from_parsed(unresolved),
        schemas,
    )
    .expect("test block-name resolution must succeed")
}

fn build_override_aware_pair_for_test(
    unresolved: CanonicalResourcePair<carina_core::parser::ParsedExportParam>,
    state_file: Option<&StateFile>,
    inputs: LivePreApplyInputs<'_>,
) -> Result<(OverrideAwareResources, OverrideAwareResources), String> {
    let state = ProjectedStateRead::for_test_unmoved(state_file);
    OverrideAwareResources::build_pair_with_stability(
        &unresolved,
        &state,
        inputs,
        &HashMap::new(),
        OverrideAwareBuildMode::Apply,
    )
}

fn resolved_provider_configs<E>(
    parsed: &carina_core::parser::File<E>,
) -> carina_core::parser::ResolvedProviderConfigs {
    resolved_provider_configs_with_remote(parsed, &HashMap::new())
}

fn tied_union_error_with_attributes(first: &'static str, second: &'static str) -> TypeError {
    let rejecting_member = |attribute: &'static str, pattern: &'static str| {
        AttributeType::refined_string_with_validator(
            TypeIdentity::bare("TestValidator"),
            None,
            None,
            carina_core::schema::validator(move |_| {
                Err(TypeError::PatternMismatch {
                    value: "invalid".into(),
                    pattern: pattern.to_string(),
                    attribute: Some(attribute.to_string()),
                    type_name: None,
                })
            }),
            None,
        )
    };
    let union = AttributeType::union(vec![
        rejecting_member(first, "^first$"),
        rejecting_member(second, "^second$"),
    ]);

    pathless_test_type(union)
        .validate(&Value::Concrete(ConcreteValue::String("invalid".into())))
        .expect_err("both union members must reject the value")
}

fn tied_union_error_with_one_unattributed_failure() -> TypeError {
    let attributed = AttributeType::refined_string_with_validator(
        TypeIdentity::bare("TestValidator"),
        None,
        None,
        carina_core::schema::validator(|_| {
            Err(TypeError::PatternMismatch {
                value: "invalid".into(),
                pattern: "^first$".to_string(),
                attribute: Some("target".to_string()),
                type_name: None,
            })
        }),
        None,
    );
    let unattributed = AttributeType::refined_string_with_validator(
        TypeIdentity::bare("TestValidator"),
        None,
        None,
        carina_core::schema::validator(|_| {
            Err(TypeError::ValidationFailed {
                message: "second failure".to_string(),
            })
        }),
        None,
    );

    pathless_test_type(AttributeType::union(vec![attributed, unattributed]))
        .validate(&Value::Concrete(ConcreteValue::String("invalid".into())))
        .expect_err("both union members must reject the value")
}

#[test]
fn tied_union_error_reports_shared_attribute() {
    let error = tied_union_error_with_attributes("target", "target");
    assert_eq!(error.attribute_name(), Some("target"));
}

#[test]
fn tied_union_error_with_different_attributes_has_no_single_attribute() {
    let error = tied_union_error_with_attributes("first", "second");
    assert_eq!(error.attribute_name(), None);
}

#[test]
fn tied_union_error_ignores_failures_without_an_attribute() {
    let error = tied_union_error_with_one_unattributed_failure();
    assert_eq!(error.attribute_name(), Some("target"));
}

#[tokio::test]
async fn shared_desired_pipeline_runs_host_default_tag_normalization() {
    let secret = Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
        ConcreteValue::String("test-only-default-tag-secret".into()),
    ))));
    let provider_config = ProviderConfig {
        name: BUILTIN_PROVIDER_NAME.to_string(),
        attributes: IndexMap::new(),
        default_tags: IndexMap::from([("SecretTag".to_string(), secret.clone())]),
        source: None,
        version: None,
        revision: None,
        unresolved_attributes: IndexMap::new(),
        binding: None,
    };
    let provider_configs = vec![provider_config];
    let ctx = WiringContext::new_for_test(Vec::new(), &provider_configs);
    let mut resources = vec![Resource::with_provider(
        BUILTIN_PROVIDER_NAME,
        "test.resource",
        "default-tag-target",
        None,
    )];

    prepare_desired_side_for_plan(
        &mut resources,
        &mut [],
        &provider_configs,
        &carina_core::provider::NoopNormalizer,
        &ctx,
    )
    .await
    .expect("test normalizer should traverse the shared desired pipeline");

    let Value::Concrete(ConcreteValue::Map(tags)) = resources[0]
        .attributes
        .get("tags")
        .expect("host default-tag normalization should merge default tags")
    else {
        panic!("merged tags must be a map")
    };
    assert_eq!(tags.get("SecretTag"), Some(&secret));
}

#[tokio::test]
async fn plan_desired_pipeline_rejects_unresolved_enums_at_every_schema_position() {
    struct EnumSchemaFactory;

    impl ProviderFactory for EnumSchemaFactory {
        fn name(&self) -> &str {
            "test"
        }

        fn display_name(&self) -> &str {
            "test"
        }

        fn provider_config_attribute_types(&self) -> carina_core::schema::ValidatedConfigSchema {
            Default::default()
        }

        fn validate_config(
            &self,
            _attributes: &carina_core::provider::CanonicalProviderConfigAttributes,
        ) -> carina_core::provider::ProviderResult<()> {
            Ok(())
        }

        fn extract_region(&self, _attributes: &IndexMap<String, Value>) -> String {
            String::new()
        }

        fn create_provider(
            &self,
            _binding: Option<&str>,
            _config: &ProviderReadyConfig,
        ) -> BoxFuture<'_, ProviderResult<Box<dyn Provider>>> {
            unreachable!("plan canonicalization test does not create providers")
        }

        fn schemas(&self) -> Vec<carina_core::schema::ValidatedResourceSchema> {
            let schema_drafts: Vec<carina_core::schema::ResourceSchema> = {
                let mode = AttributeType::enum_(
                    carina_core::schema::enum_identity("Mode", Some("test.thing")),
                    Some(vec!["Allow".to_string()]),
                    vec![("Allow".to_string(), "allow".to_string())],
                    None,
                    None,
                );
                vec![
                    ResourceSchema::new("thing")
                        .attribute(AttributeSchema::optional("top", mode.clone()).build())
                        .attribute(
                            AttributeSchema::optional(
                                "nested",
                                AttributeType::struct_(
                                    "Settings",
                                    vec![
                                        carina_core::schema::StructField::optional(
                                            "mode",
                                            mode.clone(),
                                        )
                                        .build(),
                                    ],
                                ),
                            )
                            .build(),
                        )
                        .attribute(
                            AttributeSchema::optional("items", AttributeType::list(mode.clone()))
                                .build(),
                        )
                        .attribute(
                            AttributeSchema::optional(
                                "unselected_union",
                                AttributeType::union(vec![
                                    AttributeType::list(mode),
                                    AttributeType::map(AttributeType::string()),
                                ]),
                            )
                            .build(),
                        ),
                ]
            };
            schema_drafts
                .into_iter()
                .map(|schema| {
                    schema
                        .validate()
                        .expect("factory test schema must be valid")
                })
                .collect()
        }
    }

    let invalid = || {
        Value::Concrete(ConcreteValue::enum_identifier(
            "test.thing.Mode.not_a_member",
        ))
    };
    let cases = [
        ("top", invalid()),
        (
            "nested",
            Value::Concrete(ConcreteValue::Map(IndexMap::from([(
                "mode".to_string(),
                invalid(),
            )]))),
        ),
        (
            "items",
            Value::Concrete(ConcreteValue::List(vec![invalid()])),
        ),
        ("unselected_union", invalid()),
    ];

    for (attribute, value) in cases {
        let provider_configs = Vec::new();
        let ctx = WiringContext::new_for_test(vec![Box::new(EnumSchemaFactory)], &provider_configs);
        let mut resources = vec![
            Resource::with_provider("test", "thing", attribute, None)
                .with_attribute(attribute, value),
        ];

        let error = prepare_desired_side_for_plan(
            &mut resources,
            &mut [],
            &provider_configs,
            &carina_core::provider::NoopNormalizer,
            &ctx,
        )
        .await
        .expect_err("an unresolved enum must abort plan desired preprocessing");

        assert!(error.to_string().contains(attribute), "{error}");
        assert!(error.detail().cause().is_some());
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn typed_provider_name_mismatches_map_to_fatal_source_errors() {
    let user_dirs = carina_core::TestUserDirs::new();
    let tmp = tempfile::tempdir().unwrap();
    let wasm_path = tmp.path().join("provider.wasm");
    std::fs::write(&wasm_path, b"fixture").unwrap();
    let source: ProviderSource = format!("file://{}", wasm_path.display()).parse().unwrap();
    let config = ProviderConfig {
        name: "configured".into(),
        source: Some(source.clone()),
        version: None,
        revision: None,
        unresolved_attributes: IndexMap::new(),
        binding: None,
        attributes: IndexMap::new(),
        default_tags: IndexMap::new(),
    };
    let installed =
        carina_provider_resolver::find_file_provider_source(tmp.path(), &config).unwrap();

    let reported_mismatch = installed.with_load_error(
        carina_plugin_host::WasmProviderLoadError::ProviderNameMismatch {
            configured: "configured".into(),
            reported: "reported".into(),
        },
    );
    let reported = source_provider_kind_mismatch_from_load_error(&source, &reported_mismatch)
        .expect("an artifact-reported name mismatch must be fatal");
    assert_eq!(
        reported.to_string(),
        format!(
            "Provider block kind 'configured' does not match loaded provider name 'reported' from source '{source}'."
        )
    );

    let grants = carina_provider_resolver::TrustedLocalGrants::parse(
        &[format!("{}=aws", wasm_path.display())],
        None,
    )
    .unwrap();
    let identity_mismatch = installed
        .load_with(&user_dirs, &grants, |_verified| async {
            Ok::<_, carina_plugin_host::WasmProviderLoadError>(())
        })
        .await
        .unwrap_err();
    let identity = source_provider_kind_mismatch_from_load_error(&source, &identity_mismatch)
        .expect("a trusted-identity name mismatch must be fatal");
    assert_eq!(
        identity.to_string(),
        format!(
            "Provider block kind 'configured' does not match trusted provider identity 'aws or awscc' from source '{source}'."
        )
    );

    let installed_path = tmp
        .path()
        .join(".carina/providers/file/provider/provider.wasm");
    std::fs::create_dir_all(installed_path.parent().unwrap()).unwrap();
    std::fs::write(&installed_path, b"fixture").unwrap();
    let inputs = TrustedLocalGrantInputs::new(Vec::new(), None);
    let result = build_factories_from_providers_impl_using(
        &user_dirs,
        std::slice::from_ref(&config),
        tmp.path(),
        true,
        &inputs,
        |_verified| async {
            Err(
                carina_plugin_host::WasmProviderLoadError::ProviderNameMismatch {
                    configured: "configured".into(),
                    reported: "reported".into(),
                },
            )
        },
    );
    let Err(error) = result else {
        panic!("artifact-reported provider kind mismatch must be fatal")
    };
    assert!(matches!(error, AppError::SourceProviderKindMismatch { .. }));

    let trusted_inputs =
        TrustedLocalGrantInputs::new(vec![format!("{}=aws", wasm_path.display())], None);
    let result = build_factories_from_providers_impl_using(
        &user_dirs,
        std::slice::from_ref(&config),
        tmp.path(),
        true,
        &trusted_inputs,
        |_verified| async { panic!("trusted identity mismatch must fail before the WASM loader") },
    );
    let Err(error) = result else {
        panic!("trusted provider kind mismatch must be fatal")
    };
    assert!(matches!(
        error,
        AppError::SourceProviderTrustMismatch { .. }
    ));

    let (factories, errors) = build_factories_from_providers_impl_using(
        &user_dirs,
        std::slice::from_ref(&config),
        tmp.path(),
        false,
        &inputs,
        |_verified| async {
            Err(
                carina_plugin_host::WasmProviderLoadError::ProviderNameMismatch {
                    configured: "configured".into(),
                    reported: "reported".into(),
                },
            )
        },
    )
    .unwrap();
    assert!(factories.is_empty());
    assert!(
        errors
            .get("configured")
            .is_some_and(|message| message.contains("artifact-reported name 'reported'")),
        "the factory-discovery path must retain mismatches as load diagnostics: {errors:?}"
    );
}

#[test]
fn unverified_official_factory_load_is_fatal_before_loader() {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(unverified_official_factory_load_case());
}

async fn unverified_official_factory_load_case() {
    let user_dirs = carina_core::TestUserDirs::new();
    let project = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(user_dirs.trust_record_dir()).unwrap();
    let source = "github.com/carina-rs/carina-provider-aws";
    let version = "0.1.0";
    let provider_source: ProviderSource = source.parse().unwrap();
    let ProviderSource::Remote(remote_source) = &provider_source else {
        panic!("official source must be remote")
    };
    let wasm_path = carina_provider_resolver::cache_path_wasm(
        project.path(),
        &remote_source.source_key(),
        version,
    );
    std::fs::create_dir_all(wasm_path.parent().unwrap()).unwrap();
    std::fs::write(&wasm_path, b"official provider fixture").unwrap();
    let mut lock = carina_provider_resolver::LockFile::default();
    lock.upsert(carina_provider_resolver::LockEntry {
        name: "aws".into(),
        source: source.into(),
        kind: carina_provider_resolver::LockEntryKind::GithubRelease {
            version: version.parse().unwrap(),
            tag: serde_json::from_str(&format!("\"v{version}\"")).unwrap(),
            asset_id: serde_json::from_str("42").unwrap(),
            constraint: None,
        },
        sha256: carina_provider_resolver::sha256_file(&wasm_path).unwrap(),
        registry: None,
    });
    lock.save(&project.path().join("carina-providers.lock"))
        .unwrap();
    let config = ProviderConfig {
        name: "aws".into(),
        source: Some(provider_source),
        version: None,
        revision: None,
        unresolved_attributes: IndexMap::new(),
        binding: None,
        attributes: IndexMap::new(),
        default_tags: IndexMap::new(),
    };
    let loader_called = Arc::new(AtomicBool::new(false));
    let called = Arc::clone(&loader_called);
    let inputs = TrustedLocalGrantInputs::new(Vec::new(), None);

    let result = build_factories_from_providers_impl_using(
        &user_dirs,
        std::slice::from_ref(&config),
        project.path(),
        false,
        &inputs,
        move |_verified| {
            called.store(true, Ordering::SeqCst);
            async {
                Err(carina_plugin_host::WasmProviderLoadError::Other(
                    "loader must not run".into(),
                ))
            }
        },
    );

    assert!(!loader_called.load(Ordering::SeqCst));
    let Err(error) = result else {
        panic!("unverified official providers must be fatal")
    };
    let rendered = error.to_string();
    assert!(
        rendered.starts_with(&format!(
            "{source} is a carina-rs provider but this user hasn't verified it; run `carina init {}`",
            project.path().display()
        )),
        "{rendered}"
    );
    assert!(
        std::error::Error::source(&error).is_none(),
        "the transparent AppError must not re-expose the self-contained diagnostic"
    );
    assert!(matches!(error, AppError::UnverifiedOfficialProvider(_)));
    assert!(
        std::fs::read_dir(user_dirs.trust_record_dir())
            .unwrap()
            .next()
            .is_none(),
        "a rejected artifact must not write a trust record"
    );
}

fn resolved_provider_configs_with_remote<E>(
    parsed: &carina_core::parser::File<E>,
    remote: &HashMap<String, HashMap<String, Value>>,
) -> carina_core::parser::ResolvedProviderConfigs {
    carina_core::parser::resolve_provider_attributes_with_remote(
        parsed,
        remote,
        &carina_core::parser::ProviderContext::default(),
    )
    .expect("test provider configs must resolve")
}

fn validated_provider_configs(
    configs: Vec<ProviderConfig>,
) -> carina_core::parser::ResolvedProviderConfigs {
    let mut parsed = carina_core::parser::InferredFile::default();
    parsed.providers = configs;
    resolved_provider_configs(&parsed)
}

#[allow(clippy::too_many_arguments)]
async fn create_plan_from_parsed_with_upstream_for_test<E: Clone>(
    parsed: &carina_core::parser::File<E>,
    provider_configs: &carina_core::parser::ResolvedProviderConfigs,
    unresolved_resources: &[Resource],
    state_file: &Option<StateFile>,
    refresh: bool,
    remote_bindings: &HashMap<String, HashMap<String, Value>>,
    state_block_claims: &StateBlockClaims,
    resolved_state_block_targets: &ResolvedStateBlockTargets,
    base_dir: &Path,
) -> Result<PlanContext, AppError> {
    let user_dirs = carina_core::TestUserDirs::new();
    let (factories, _) = build_factories_from_providers(
        &user_dirs,
        provider_configs.as_provider_configs(),
        base_dir,
    )?;
    let wiring = ValidatedWiring::for_test(factories, provider_configs.as_provider_configs());
    let ctx = wiring.context();
    let mut unresolved = carina_core::parser::ParsedFile::default();
    unresolved.resources = unresolved_resources.to_vec();
    let canonical_resources = resolve_config_block_names_for_test(
        parsed.clone(),
        carina_core::config_loader::UnresolvedConfigSnapshot::from_parsed(unresolved),
        ctx.schemas(),
    )
    .expect("test block-name resolution must succeed");
    let validated = ValidatedConfig::from_wiring_ref_for_test(&wiring, canonical_resources);
    create_plan_from_parsed_with_upstream_with_ctx(
        validated,
        provider_configs,
        state_file,
        refresh,
        ProgressSink::Human,
        remote_bindings,
        state_block_claims,
        resolved_state_block_targets,
    )
    .await
    .map(ValidatedConfig::into_context_for_test)
}

#[allow(clippy::too_many_arguments)]
fn create_plan(
    managed: &[Resource],
    data_sources: &[carina_core::resource::DataSource],
    provider: &dyn Provider,
    current_states: &HashMap<ResourceId, carina_core::resource::PlanInputCurrent>,
    directives_map: &HashMap<ResourceId, Directives>,
    registry: &SchemaRegistry,
    saved_attrs: &carina_core::provider::LiftedSavedAttrs,
    prev_explicit: &HashMap<ResourceId, carina_core::explicit::ExplicitFields>,
    orphans: &Orphans,
    wait_bindings: &[carina_core::parser::WaitBinding],
) -> Plan {
    let managed = ResolvedResource::collect_resolved(managed.iter().cloned()).unwrap();
    carina_core::differ::create_plan(
        &managed,
        data_sources,
        provider,
        carina_core::differ::PlanPriorStates::new(current_states, orphans),
        directives_map,
        registry,
        saved_attrs,
        prev_explicit,
        wait_bindings,
    )
    .expect("test provider hints should succeed")
}

fn test_identity(value: impl Into<String>) -> ResourceIdentity {
    ResourceIdentity::try_from(value.into()).unwrap()
}

struct ReadWithRetryProvider {
    behavior: ReadBehavior,
    read_calls: AtomicUsize,
    data_source_read_calls: AtomicUsize,
}

struct NoopExecutionObserver;

impl ExecutionObserver for NoopExecutionObserver {
    fn on_event(&self, _event: &ExecutionEvent) {}
}

#[tokio::test]
async fn plan_preprocessor_propagates_normalizer_failure() {
    struct FailingNormalizer {
        state_called: Arc<AtomicBool>,
    }

    impl ProviderNormalizer for FailingNormalizer {
        fn normalize_desired<'a>(
            &'a self,
            _resources: &'a mut [carina_core::resource::ProviderResource],
        ) -> BoxFuture<'a, ProviderResult<()>> {
            Box::pin(async { Err(ProviderError::internal("plan normalization failed")) })
        }

        fn normalize_state<'a>(
            &'a self,
            _current_states: &'a mut carina_core::provider::StateTransformMap,
        ) -> BoxFuture<'a, ProviderResult<()>> {
            Box::pin(async move {
                self.state_called.store(true, Ordering::SeqCst);
                Ok(())
            })
        }

        fn hydrate_read_state<'a>(
            &'a self,
            _current_states: &'a mut carina_core::provider::StateTransformMap,
            _saved_attrs: &'a carina_core::provider::SavedAttrs,
        ) -> BoxFuture<'a, ProviderResult<()>> {
            carina_core::provider::ready_noop()
        }
    }

    let state_called = Arc::new(AtomicBool::new(false));
    let normalizer = FailingNormalizer {
        state_called: Arc::clone(&state_called),
    };
    let ctx = WiringContext::new_for_test(Vec::new(), &[]);
    let (mut resources, _) = build_override_aware_pair_for_test(
        canonical_unresolved(Vec::new(), ctx.schemas()),
        None::<&StateFile>,
        carina_core::binding_index::LivePreApplyInputs {
            managed: &[],
            compositions: &[],
            managed_current: &HashMap::new(),
            data_source_values: &DataSourceValues::empty(),
            remote_bindings: &HashMap::new(),
            wait_aliases: &[],
        },
    )
    .unwrap();
    let module_gate = carina_core::executor::ModuleConstraintGate::from_live_compositions(&[]);
    let mut current_states = HashMap::new();
    let mut data_source_values = DataSourceValues::empty();
    let mut orphans = Orphans::new();

    let error = PlanPreprocessor::new(&normalizer, &ctx)
        .prepare(
            &mut resources,
            &[],
            &module_gate,
            &mut current_states,
            &mut data_source_values,
            &mut orphans,
            &[],
            &mut [],
            &[],
        )
        .await
        .expect_err("normalizer failure must abort plan preprocessing");

    match error {
        PlanPreparationError::Provider(error) => {
            assert_eq!(error.to_string(), "plan normalization failed");
        }
        PlanPreparationError::Plan(errors) => panic!("unexpected plan errors: {errors:#?}"),
    }
    assert!(
        !state_called.load(Ordering::SeqCst),
        "state normalization must not run after desired normalization fails"
    );
}

#[test]
fn provider_plan_preparation_error_chain_renders_leaf_once() {
    let error = PlanPreparationError::Provider(ProviderError::api_error_caused_by(
        "x",
        std::io::Error::other("provider leaf reason"),
    ));

    let rendered = error.to_string();

    assert_eq!(
        rendered.matches("provider leaf reason").count(),
        1,
        "{rendered}"
    );
}

#[tokio::test]
async fn post_reconcile_managed_set_promotes_transformed_orphan_for_bindings_and_diff() {
    struct TransformingNormalizer;

    impl ProviderNormalizer for TransformingNormalizer {
        fn normalize_desired<'a>(
            &'a self,
            _resources: &'a mut [carina_core::resource::ProviderResource],
        ) -> BoxFuture<'a, ProviderResult<()>> {
            carina_core::provider::ready_noop()
        }

        fn normalize_state<'a>(
            &'a self,
            states: &'a mut carina_core::provider::StateTransformMap,
        ) -> BoxFuture<'a, ProviderResult<()>> {
            for state in states.values_mut().filter(|state| state.exists) {
                if state.attributes.get("status")
                    == Some(&Value::Concrete(ConcreteValue::String(
                        "provider-alias".into(),
                    )))
                {
                    state.attributes.insert(
                        "status".to_string(),
                        Value::Concrete(ConcreteValue::String("canonical".into())),
                    );
                }
            }
            carina_core::provider::ready_noop()
        }

        fn hydrate_read_state<'a>(
            &'a self,
            states: &'a mut carina_core::provider::StateTransformMap,
            _saved_attrs: &'a carina_core::provider::SavedAttrs,
        ) -> BoxFuture<'a, ProviderResult<()>> {
            for state in states.values_mut().filter(|state| state.exists) {
                state.attributes.insert(
                    "restored".to_string(),
                    Value::Concrete(ConcreteValue::String("saved".into())),
                );
            }
            carina_core::provider::ready_noop()
        }
    }

    let old_id = ResourceId::with_provider_identity("mock", "test.resource", "old", None);
    let target_id = ResourceId::with_provider_identity("mock", "test.resource", "target", None);
    let mut desired = Resource::with_provider("mock", "test.resource", "target", None)
        .with_attribute(
            "status",
            Value::Concrete(ConcreteValue::String("canonical".into())),
        )
        .with_attribute(
            "restored",
            Value::Concrete(ConcreteValue::String("saved".into())),
        );
    desired.binding = Some("resource".to_string());
    let (mut resources, _) = build_override_aware_pair_for_test(
        canonical_unresolved(vec![desired], &SchemaRegistry::new()),
        None::<&StateFile>,
        LivePreApplyInputs {
            managed: &[],
            compositions: &[],
            managed_current: &HashMap::new(),
            data_source_values: &DataSourceValues::empty(),
            remote_bindings: &HashMap::new(),
            wait_aliases: &[],
        },
    )
    .unwrap();
    let resource_origins = resources.paired_unresolved_resources();
    let mut current_states = HashMap::new();
    let orphan_state = State::existing(
        old_id.clone(),
        HashMap::from([
            (
                "status".to_string(),
                Value::Concrete(ConcreteValue::String("provider-alias".into())),
            ),
            (
                "observed".to_string(),
                Value::Concrete(ConcreteValue::String("from-provider".into())),
            ),
        ]),
    )
    .with_identifier("resource-123");
    let present = match Current::from_state(orphan_state) {
        Current::Absent(state) => panic!("test orphan is absent: {state:?}"),
        Current::Present(present) => present,
        Current::MissingIdentifier(state) => {
            panic!("test orphan lacks a provider identifier: {state:?}")
        }
        Current::CreateInFlight(pending) => {
            panic!("test orphan unexpectedly has an in-flight create: {pending:?}")
        }
    };
    let mut orphans = Orphans::from([carina_core::differ::Orphan::present(
        present,
        Some("resource".to_string()),
        Directives::default(),
    )]);
    let normalizer = TransformingNormalizer;
    let saved_attrs = crate::empty_lifted_saved_attrs();
    let data_source_id = ResourceId::with_provider_identity("mock", "test.lookup", "source", None);
    let data_source_states = HashMap::from([(
        data_source_id.clone(),
        State::existing(
            data_source_id.clone(),
            HashMap::from([(
                "status".to_string(),
                Value::Concrete(ConcreteValue::String("provider-alias".into())),
            )]),
        ),
    )]);
    let data_source = DataSource::with_provider("mock", "test.lookup", "source", None);
    let mut data_source_values =
        test_data_source_values(std::slice::from_ref(&data_source), &data_source_states);
    let projected_state = ProjectedStateRead::for_test_unmoved(None);

    hydrate_prior_data_source_values(
        &mut current_states,
        &mut data_source_values,
        &mut orphans,
        &normalizer,
        &saved_attrs,
        &projected_state,
    )
    .await
    .unwrap();
    PlanPreprocessor::new(&normalizer, &WiringContext::new_for_test(Vec::new(), &[]))
        .prepare(
            &mut resources,
            &resource_origins,
            &carina_core::executor::ModuleConstraintGate::from_live_compositions(&[]),
            &mut current_states,
            &mut data_source_values,
            &mut orphans,
            &[],
            &mut [],
            &[],
        )
        .await
        .unwrap();

    let data_source_state = data_source_values
        .read_states()
        .find_map(|(id, state)| (id == &data_source_id).then_some(state))
        .expect("configured data source remains read");
    assert_eq!(
        data_source_state.attributes.get("status"),
        Some(&Value::Concrete(ConcreteValue::String("canonical".into())))
    );
    assert_eq!(
        data_source_state.attributes.get("restored"),
        Some(&Value::Concrete(ConcreteValue::String("saved".into())))
    );
    assert!(data_source_state.identifier.is_none());

    let move_effect: Effect = Effect::Move {
        from: ResolvedResourceId::new(old_id.clone()),
        to: ResolvedResourceId::new(target_id.clone()),
    };
    let authority = move_effect
        .move_authority()
        .expect("the fixture effect is a move");
    assert!(orphans.rekey(&authority));

    let provider_states =
        carina_core::differ::provider_states_for(&current_states, &orphans, resources.resources());
    let input_states = carina_core::resource::into_plan_input_current_map(
        provider_states,
        &SchemaRegistry::new(),
        resources.resources(),
    );
    resources.rebuild_bindings_after_identity_reconciliation(RebuildBindingsInputs {
        compositions: &[],
        managed_current: &input_states,
        data_source_values: &DataSourceValues::empty(),
        remote_bindings: &HashMap::new(),
        wait_aliases: &[],
    });
    assert_eq!(
        resources
            .bindings()
            .get("resource")
            .and_then(|attrs| attrs.get("observed")),
        Some(&Value::Concrete(ConcreteValue::String(
            "from-provider".into()
        ))),
        "post-reconcile bindings must derive from the same managed set as the differ",
    );
    let plan = create_plan(
        resources.resources(),
        &[],
        &carina_core::provider::ProviderRouter::new(),
        &carina_core::resource::into_plan_input_current_map::<carina_core::resource::ModuleSource>(
            current_states,
            &SchemaRegistry::new(),
            resources.resources(),
        ),
        &HashMap::new(),
        &SchemaRegistry::new(),
        &saved_attrs,
        &HashMap::new(),
        &orphans,
        &[],
    );

    assert!(
        plan.effects().is_empty(),
        "a promoted orphan transformed by every prior-state stage must be a no-op, got {:?}",
        plan.effects()
    );
}

#[tokio::test]
async fn combined_hydration_keeps_managed_state_out_of_same_id_data_source() {
    let id = ResourceId::with_provider_identity("mock", "test.resource", "orphan", None);
    let marker = carina_core::resource::PartialReadMarker::new(
        carina_core::resource::PendingReRead::incomplete("provider read omitted token"),
        BTreeSet::from(["token".to_string()]),
    )
    .expect("fixture has a missing attribute");
    let mut row = carina_state::ResourceState::try_applied(
        &id.resource_type,
        id.identity().expect("resolved test id").clone(),
        &id.provider,
        "resource-123",
    )
    .expect("fixture identifier is non-empty");
    row.binding = Some("orphan".to_string());
    row.set_persisted_attribute("managed_saved", serde_json::json!("saved-value"));
    row = crate::fixture_resource_state_metadata(
        row,
        Some(vec!["managed_saved".to_string()]),
        Some(Some(marker.clone())),
    );
    let mut state_file = StateFile::new();
    state_file
        .upsert_resource(row)
        .expect("test state setup must be valid");
    let saved_attrs = state_file.build_saved_attrs().lift(&SchemaRegistry::new());

    let mut orphans = state_file
        .build_orphans(&HashSet::new())
        .into_plan_orphans();
    let orphan = orphans.remove(&id).expect("orphan row");
    let refreshed = State::existing(id.clone(), HashMap::new()).with_identifier("resource-123");
    orphans.insert(orphan.with_refreshed_state(refreshed));
    let state = match orphans[&id].current() {
        Current::Absent(state) => state.state(),
        Current::Present(present) => present.state(),
        Current::MissingIdentifier(state) => state.state(),
        Current::CreateInFlight(pending) => {
            panic!("orphan unexpectedly remained in flight: {pending:?}")
        }
    };
    assert_eq!(state.partial_read, None);

    let mut current_states = HashMap::new();
    let data_source = DataSource::with_provider("mock", "test.resource", "orphan", None);
    let mut data_source_values =
        DataSourceValues::from_data_sources(std::slice::from_ref(&data_source), |_| {
            DataSourceValue::read(State::existing(id.clone(), HashMap::new()))
        });
    let projected_state = ProjectedStateRead::for_test_unmoved(Some(&state_file));
    hydrate_prior_data_source_values(
        &mut current_states,
        &mut data_source_values,
        &mut orphans,
        &SavedAttrsHydratingNormalizer,
        &saved_attrs,
        &projected_state,
    )
    .await
    .expect("combined state hydration");

    let state = match orphans[&id].current() {
        Current::Absent(state) => state.state(),
        Current::Present(present) => present.state(),
        Current::MissingIdentifier(state) => state.state(),
        Current::CreateInFlight(pending) => {
            panic!("orphan unexpectedly remained in flight: {pending:?}")
        }
    };
    assert_eq!(state.partial_read, Some(marker));
    assert_eq!(
        state.attributes.get("managed_saved"),
        Some(&Value::Concrete(ConcreteValue::String(
            "saved-value".into()
        )))
    );
    let data_source_state = data_source_values
        .read_states()
        .find_map(|(read_id, state)| (read_id == &id).then_some(state))
        .expect("configured data source remains read");
    assert!(!data_source_state.attributes.contains_key("managed_saved"));
    assert_eq!(data_source_state.partial_read, None);
    assert!(current_states.is_empty());
}

#[tokio::test]
async fn instantiate_provider_into_router_propagates_normalizer_creation_failure() {
    struct FailingNormalizerFactory;

    impl ProviderFactory for FailingNormalizerFactory {
        fn name(&self) -> &str {
            "failing"
        }

        fn display_name(&self) -> &str {
            "Failing normalizer factory"
        }

        fn provider_config_attribute_types(&self) -> carina_core::schema::ValidatedConfigSchema {
            Default::default()
        }

        fn validate_config(
            &self,
            _attributes: &carina_core::provider::CanonicalProviderConfigAttributes,
        ) -> carina_core::provider::ProviderResult<()> {
            Ok(())
        }

        fn extract_region(&self, _attributes: &IndexMap<String, Value>) -> String {
            "test-region".to_string()
        }

        fn create_provider(
            &self,
            _binding: Option<&str>,
            _config: &carina_core::provider::ProviderReadyConfig,
        ) -> BoxFuture<'_, ProviderResult<Box<dyn Provider>>> {
            Box::pin(async { Ok(Box::new(MockProvider::new()) as Box<dyn Provider>) })
        }

        fn create_normalizer(
            &self,
            _binding: Option<&str>,
            _config: &carina_core::provider::ProviderReadyConfig,
        ) -> BoxFuture<'_, ProviderResult<Box<dyn ProviderNormalizer>>> {
            Box::pin(async { Err(ProviderError::internal("normalizer initialization failed")) })
        }

        fn schemas(&self) -> Vec<carina_core::schema::ValidatedResourceSchema> {
            let schema_drafts: Vec<carina_core::schema::ResourceSchema> = Vec::new();
            schema_drafts
                .into_iter()
                .map(|schema| {
                    schema
                        .validate()
                        .expect("factory test schema must be valid")
                })
                .collect()
        }
    }

    let ctx = WiringContext::new_for_test(vec![Box::new(FailingNormalizerFactory)], &[]);
    let config = ProviderConfig {
        name: "failing".to_string(),
        attributes: IndexMap::new(),
        default_tags: IndexMap::new(),
        source: None,
        version: None,
        revision: None,
        unresolved_attributes: IndexMap::new(),
        binding: None,
    };
    let configs = validated_provider_configs(vec![config]);
    let config = configs.iter().next().unwrap();
    let mut router = ProviderRouter::new();
    let error =
        instantiate_provider_into_router(&ctx, &mut router, config, None, ProgressSink::Human)
            .await
            .expect_err("normalizer creation failure must abort provider router setup");

    let AppError::Provider(error) = error else {
        panic!("expected provider error, got {error}");
    };
    assert_eq!(error.to_string(), "normalizer initialization failed");
    assert_eq!(error.detail().provider_name.as_deref(), Some("failing"));
}

#[test]
fn named_provider_reports_unavailable_sourced_default() {
    let configs = validated_provider_configs(vec![ProviderConfig {
        name: "missing".to_string(),
        attributes: IndexMap::new(),
        default_tags: IndexMap::new(),
        source: None,
        version: None,
        revision: None,
        unresolved_attributes: IndexMap::new(),
        binding: Some("named".to_string()),
    }]);
    let source: ProviderSource = "file:///missing.wasm".parse().unwrap();

    assert_eq!(
        missing_provider_message(configs.iter().next().unwrap(), Some(&source)).as_deref(),
        Some(
            "Named provider instance 'named' (kind 'missing') cannot be loaded because the kind's default instance is unavailable."
        )
    );
}

#[tokio::test]
async fn provider_progress_is_emitted_after_provider_initialization() {
    struct ProgressOrderingFactory {
        lines: &'static RecordedProgressLines,
    }

    impl ProviderFactory for ProgressOrderingFactory {
        fn name(&self) -> &str {
            "ordering"
        }

        fn display_name(&self) -> &str {
            "Progress ordering factory"
        }

        fn provider_config_attribute_types(&self) -> carina_core::schema::ValidatedConfigSchema {
            Default::default()
        }

        fn validate_config(
            &self,
            _attributes: &carina_core::provider::CanonicalProviderConfigAttributes,
        ) -> ProviderResult<()> {
            Ok(())
        }

        fn extract_region(&self, _attributes: &IndexMap<String, Value>) -> String {
            "test-region".to_string()
        }

        fn create_provider(
            &self,
            _binding: Option<&str>,
            _config: &carina_core::provider::ProviderReadyConfig,
        ) -> BoxFuture<'_, ProviderResult<Box<dyn Provider>>> {
            Box::pin(async move {
                if self.lines.is_empty() {
                    Ok(Box::new(MockProvider::new()) as Box<dyn Provider>)
                } else {
                    Err(ProviderError::internal(
                        "provider progress was emitted before initialization",
                    ))
                }
            })
        }

        fn schemas(&self) -> Vec<carina_core::schema::ValidatedResourceSchema> {
            Vec::new()
        }
    }

    let lines = Box::leak(Box::new(RecordedProgressLines::default()));
    let ctx = WiringContext::new_for_test(vec![Box::new(ProgressOrderingFactory { lines })], &[]);
    let configs = validated_provider_configs(vec![ProviderConfig {
        name: "ordering".to_string(),
        attributes: IndexMap::new(),
        default_tags: IndexMap::new(),
        source: None,
        version: None,
        revision: None,
        unresolved_attributes: IndexMap::new(),
        binding: None,
    }]);
    let mut router = ProviderRouter::new();

    instantiate_provider_into_router(
        &ctx,
        &mut router,
        configs.iter().next().unwrap(),
        None,
        ProgressSink::Recorded(lines),
    )
    .await
    .expect("provider initialization must precede its progress line");

    assert_eq!(lines.len(), 1);
}

#[tokio::test]
async fn sourced_provider_initialization_error_keeps_provider_and_phase() {
    struct FailingInitializeFactory;

    impl ProviderFactory for FailingInitializeFactory {
        fn name(&self) -> &str {
            "aws"
        }

        fn display_name(&self) -> &str {
            "Failing initialize factory"
        }

        fn provider_config_attribute_types(&self) -> carina_core::schema::ValidatedConfigSchema {
            Default::default()
        }

        fn validate_config(
            &self,
            _attributes: &carina_core::provider::CanonicalProviderConfigAttributes,
        ) -> carina_core::provider::ProviderResult<()> {
            Ok(())
        }

        fn extract_region(&self, _attributes: &IndexMap<String, Value>) -> String {
            "test-region".to_string()
        }

        fn create_provider(
            &self,
            _binding: Option<&str>,
            _config: &carina_core::provider::ProviderReadyConfig,
        ) -> BoxFuture<'_, ProviderResult<Box<dyn Provider>>> {
            Box::pin(async {
                Err(
                    carina_plugin_host::wasm_factory::decode_initialize_string_error_for_test(
                        "initialization rejected",
                    ),
                )
            })
        }

        fn schemas(&self) -> Vec<carina_core::schema::ValidatedResourceSchema> {
            let schema_drafts: Vec<carina_core::schema::ResourceSchema> = Vec::new();
            schema_drafts
                .into_iter()
                .map(|schema| {
                    schema
                        .validate()
                        .expect("factory test schema must be valid")
                })
                .collect()
        }
    }

    let ctx = WiringContext::new_for_test(vec![Box::new(FailingInitializeFactory)], &[]);
    let configs = validated_provider_configs(vec![ProviderConfig {
        name: "aws".to_string(),
        attributes: IndexMap::new(),
        default_tags: IndexMap::new(),
        source: Some("file:///provider.wasm".parse().unwrap()),
        version: None,
        revision: None,
        unresolved_attributes: IndexMap::new(),
        binding: None,
    }]);
    let mut router = ProviderRouter::new();
    let error = instantiate_provider_into_router(
        &ctx,
        &mut router,
        configs.iter().next().unwrap(),
        None,
        ProgressSink::Human,
    )
    .await
    .expect_err("provider initialization must fail");

    let AppError::Provider(error) = error else {
        panic!("expected provider error")
    };
    assert_eq!(error.detail().provider_name.as_deref(), Some("aws"));
    assert_eq!(error.detail().operation.as_deref(), Some("initialize"));
}

struct ProviderConfigConstraintFactory {
    creates: Arc<AtomicUsize>,
}

impl ProviderFactory for ProviderConfigConstraintFactory {
    fn name(&self) -> &str {
        "test"
    }

    fn display_name(&self) -> &str {
        "provider config constraint test provider"
    }

    fn provider_config_attribute_types(&self) -> carina_core::schema::ValidatedConfigSchema {
        carina_core::schema::ValidatedConfigSchema::validate(BTreeMap::from([(
            "role_arn".to_string(),
            AttributeType::refined_string(
                None,
                Some(r"^arn:aws:iam::[0-9]{12}:role/.+$".to_string()),
                None,
                None,
            ),
        )]))
        .expect("test provider config schema must be valid")
    }

    fn validate_config(
        &self,
        _attributes: &carina_core::provider::CanonicalProviderConfigAttributes,
    ) -> carina_core::provider::ProviderResult<()> {
        Ok(())
    }

    fn extract_region(&self, _attributes: &IndexMap<String, Value>) -> String {
        "local".to_string()
    }

    fn create_provider(
        &self,
        _binding: Option<&str>,
        _config: &carina_core::provider::ProviderReadyConfig,
    ) -> BoxFuture<'_, ProviderResult<Box<dyn Provider>>> {
        self.creates.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(Box::new(MockProvider::new()) as Box<dyn Provider>) })
    }

    fn schemas(&self) -> Vec<carina_core::schema::ValidatedResourceSchema> {
        let schema_drafts: Vec<carina_core::schema::ResourceSchema> = Vec::new();
        schema_drafts
            .into_iter()
            .map(|schema| {
                schema
                    .validate()
                    .expect("factory test schema must be valid")
            })
            .collect()
    }
}

#[tokio::test]
async fn sourced_default_provider_executes_the_factory_retained_by_wiring_context() {
    let project = tempfile::tempdir().unwrap();
    let source_path = project.path().join("provider.wasm");
    std::fs::write(&source_path, b"artifact loaded into the context").unwrap();
    let config = ProviderConfig {
        name: "test".to_string(),
        attributes: IndexMap::new(),
        default_tags: IndexMap::new(),
        source: Some(format!("file://{}", source_path.display()).parse().unwrap()),
        version: None,
        revision: None,
        unresolved_attributes: IndexMap::new(),
        binding: None,
    };
    let configs = validated_provider_configs(vec![config]);
    let creates = Arc::new(AtomicUsize::new(0));
    let ctx = WiringContext::new_for_test(
        vec![Box::new(ProviderConfigConstraintFactory {
            creates: Arc::clone(&creates),
        })],
        configs.as_provider_configs(),
    );

    std::fs::write(&source_path, b"artifact swapped after context construction").unwrap();

    let router = create_providers_from_configs_with_ctx(&ctx, &configs)
        .await
        .expect("router construction must use the already-loaded context factory");

    assert_eq!(creates.load(Ordering::SeqCst), 1);
    assert!(!router.is_empty());
}

fn resolved_invalid_provider_config() -> (
    carina_core::parser::InferredFile,
    carina_core::parser::ResolvedProviderConfigs,
) {
    let mut attributes = IndexMap::new();
    attributes.insert(
        "role_arn".to_string(),
        Value::Deferred(DeferredValue::ResourceRef {
            path: carina_core::resource::AccessPath::new("net", "role_arn"),
        }),
    );
    let mut parsed = carina_core::parser::InferredFile::default();
    parsed.providers.push(ProviderConfig {
        name: "test".to_string(),
        attributes,
        default_tags: IndexMap::new(),
        source: None,
        version: None,
        revision: None,
        unresolved_attributes: IndexMap::new(),
        binding: None,
    });
    let remote = HashMap::from([(
        "net".to_string(),
        HashMap::from([(
            "role_arn".to_string(),
            Value::Concrete(ConcreteValue::String("not-an-arn".into())),
        )]),
    )]);
    let configs = resolved_provider_configs_with_remote(&parsed, &remote);
    (parsed, configs)
}

fn assert_provider_config_constraint_error(error: &AppError) {
    let message = error.to_string();
    assert!(message.contains("provider test"), "{message}");
    assert!(message.contains("role_arn"), "{message}");
    assert!(
        message.contains(r"^arn:aws:iam::[0-9]{12}:role/.+$"),
        "{message}"
    );
}

#[tokio::test]
async fn plan_revalidates_resolved_upstream_provider_config_before_factory_creation() {
    let (parsed, provider_configs) = resolved_invalid_provider_config();
    let creates = Arc::new(AtomicUsize::new(0));
    let wiring = ValidatedWiring::from_factories_for_test(
        vec![Box::new(ProviderConfigConstraintFactory {
            creates: Arc::clone(&creates),
        })],
        provider_configs.as_provider_configs(),
    );
    let ctx = wiring.context();
    let canonical_resources = canonical_pair(&parsed, Vec::new(), ctx.schemas());
    let validated = ValidatedConfig::from_wiring_for_test(wiring, canonical_resources);
    let result = create_plan_from_parsed_with_upstream_with_ctx(
        validated,
        &provider_configs,
        &None,
        false,
        ProgressSink::Human,
        &HashMap::new(),
        &StateBlockClaims::default(),
        &ResolvedStateBlockTargets::default(),
    )
    .await;
    let error = match result {
        Ok(_) => panic!("resolved provider config must be rejected during plan"),
        Err(error) => error,
    };

    assert_provider_config_constraint_error(&error);
    assert_eq!(creates.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn apply_provider_construction_revalidates_resolved_upstream_config() {
    let (_parsed, provider_configs) = resolved_invalid_provider_config();
    let creates = Arc::new(AtomicUsize::new(0));
    let ctx = WiringContext::new_for_test(
        vec![Box::new(ProviderConfigConstraintFactory {
            creates: Arc::clone(&creates),
        })],
        provider_configs.as_provider_configs(),
    );
    let error = match get_provider_with_ctx(&ctx, &provider_configs, ProgressSink::Human).await {
        Ok(_) => panic!("resolved provider config must be rejected before apply construction"),
        Err(error) => error,
    };

    assert_provider_config_constraint_error(&error);
    assert_eq!(creates.load(Ordering::SeqCst), 0);
}

struct CascadeAwsccFactory;

impl ProviderFactory for CascadeAwsccFactory {
    fn name(&self) -> &str {
        "awscc"
    }

    fn display_name(&self) -> &str {
        "AWSCC cascade test provider"
    }

    fn provider_config_attribute_types(&self) -> carina_core::schema::ValidatedConfigSchema {
        Default::default()
    }

    fn validate_config(
        &self,
        _attributes: &carina_core::provider::CanonicalProviderConfigAttributes,
    ) -> carina_core::provider::ProviderResult<()> {
        Ok(())
    }

    fn extract_region(&self, _attributes: &IndexMap<String, Value>) -> String {
        "ap-northeast-1".to_string()
    }

    fn create_provider(
        &self,
        _binding: Option<&str>,
        _config: &carina_core::provider::ProviderReadyConfig,
    ) -> BoxFuture<'_, ProviderResult<Box<dyn Provider>>> {
        Box::pin(async { Ok(Box::new(CascadeCreateProvider::default()) as Box<dyn Provider>) })
    }

    fn schemas(&self) -> Vec<carina_core::schema::ValidatedResourceSchema> {
        let schema_drafts: Vec<carina_core::schema::ResourceSchema> = {
            vec![
                ResourceSchema::new("ec2.Vpc")
                    .attribute(
                        AttributeSchema::optional("cidr_block", AttributeType::string())
                            .create_only()
                            .build(),
                    )
                    .attribute(
                        AttributeSchema::provider_populated("vpc_id", AttributeType::string())
                            .build(),
                    )
                    .with_coexisting_replacement(),
                ResourceSchema::new("ec2.Subnet")
                    .attribute(
                        AttributeSchema::optional("vpc_id", AttributeType::string())
                            .create_only()
                            .build(),
                    )
                    .attribute(
                        AttributeSchema::optional("cidr_block", AttributeType::string())
                            .create_only()
                            .build(),
                    )
                    .attribute(
                        AttributeSchema::optional("availability_zone", AttributeType::string())
                            .create_only()
                            .build(),
                    )
                    .attribute(
                        AttributeSchema::provider_populated("subnet_id", AttributeType::string())
                            .build(),
                    )
                    .with_coexisting_replacement(),
            ]
        };
        schema_drafts
            .into_iter()
            .map(|schema| {
                schema
                    .validate()
                    .expect("factory test schema must be valid")
            })
            .collect()
    }
}

#[derive(Clone, Default)]
struct CascadeCreateProvider {
    creates: CreateCalls,
}

type CreateCall = (String, HashMap<String, Value>);
type CreateCalls = Arc<Mutex<Vec<CreateCall>>>;

impl CascadeCreateProvider {
    fn create_calls(&self) -> Vec<CreateCall> {
        self.creates.lock().unwrap().clone()
    }
}

impl Provider for CascadeCreateProvider {
    fn name(&self) -> &str {
        "cascade_create"
    }

    fn read(
        &self,
        _id: &ResourceId,
        _identifier: &AppliedIdentifier,
        _request: ReadRequest,
    ) -> BoxFuture<'_, ProviderResult<ReadOutcome>> {
        Box::pin(async { Err(ProviderError::internal("read not used")) })
    }

    fn read_data_source(
        &self,
        _resource: &carina_core::provider::ProviderReadyDataSource,
    ) -> BoxFuture<'_, ProviderResult<State>> {
        Box::pin(async { Err(ProviderError::internal("read_data_source not used")) })
    }

    fn create(
        &self,
        id: &ResourceId,
        request: CreateRequest,
        attempt: carina_core::provider::CreateAttempt,
    ) -> BoxFuture<'_, Result<CreateOutcome, carina_core::provider::CreateFailure>> {
        let id = id.clone();
        let attrs = request.resource().as_resource().resolved_attributes();
        self.creates
            .lock()
            .unwrap()
            .push((id.to_string(), attrs.clone()));

        Box::pin(async move {
            if id.resource_type == "ec2.Vpc" {
                return Ok(attempt
                    .issued(
                        AppliedIdentifier::new("vpc-new").expect("fixture identifier is non-empty"),
                        Sent::AllRequestAttributes,
                    )
                    .succeeded(HashMap::from([(
                        "vpc_id".to_string(),
                        Value::Concrete(ConcreteValue::String("vpc-new".into())),
                    )])));
            }

            if id.resource_type == "ec2.Subnet" {
                return Ok(attempt
                    .issued(
                        AppliedIdentifier::new("subnet-new")
                            .expect("fixture identifier is non-empty"),
                        Sent::AllRequestAttributes,
                    )
                    .succeeded(HashMap::from([(
                        "subnet_id".to_string(),
                        Value::Concrete(ConcreteValue::String("subnet-new".into())),
                    )])));
            }

            Err(attempt.untracked(ProviderError::internal(format!("unexpected create {id}"))))
        })
    }

    unexpected_resume_create!();

    fn update(
        &self,
        _id: &ResourceId,
        _identifier: &carina_core::provider::AppliedIdentifier,
        _request: UpdateRequest,
    ) -> BoxFuture<'_, ProviderResult<UpdateOutcome>> {
        Box::pin(async { Err(ProviderError::internal("update not used")) })
    }

    fn delete(
        &self,
        _id: &ResourceId,
        _identifier: &carina_core::provider::AppliedIdentifier,
        _request: DeleteRequest,
    ) -> BoxFuture<'_, ProviderResult<()>> {
        Box::pin(async { Ok(()) })
    }

    fn required_permissions(&self, _id: &ResourceId, _op: PlanOp) -> ProviderResult<Vec<String>> {
        Ok(Vec::new())
    }
}

impl ReadWithRetryProvider {
    fn new(behavior: ReadBehavior) -> Self {
        Self {
            behavior,
            read_calls: AtomicUsize::new(0),
            data_source_read_calls: AtomicUsize::new(0),
        }
    }

    fn read_calls(&self) -> usize {
        self.read_calls.load(Ordering::SeqCst)
    }

    fn data_source_read_calls(&self) -> usize {
        self.data_source_read_calls.load(Ordering::SeqCst)
    }
}

impl Provider for ReadWithRetryProvider {
    fn name(&self) -> &str {
        "test"
    }

    fn read(
        &self,
        id: &ResourceId,
        identifier: &AppliedIdentifier,
        _request: ReadRequest,
    ) -> BoxFuture<'_, ProviderResult<ReadOutcome>> {
        let id = id.clone();
        let identifier = identifier.clone();
        let attempt = self.read_calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            match self.behavior {
                ReadBehavior::Success => Ok(ReadOutcome::found(
                    AppliedState::new(id, identifier.as_str(), HashMap::new())
                        .expect("typed read identifier is non-empty"),
                )),
                ReadBehavior::NotFound => Err(ProviderError::not_found("missing")),
                ReadBehavior::ApiError => Err(ProviderError::api_error("boom")),
                ReadBehavior::NotFoundThenSuccess if attempt == 0 => {
                    Err(ProviderError::not_found("missing"))
                }
                ReadBehavior::NotFoundThenSuccess => Ok(ReadOutcome::found(
                    AppliedState::new(id, identifier.as_str(), HashMap::new())
                        .expect("typed read identifier is non-empty"),
                )),
            }
        })
    }

    fn read_data_source(
        &self,
        resource: &carina_core::provider::ProviderReadyDataSource,
    ) -> BoxFuture<'_, ProviderResult<State>> {
        self.data_source_read_calls.fetch_add(1, Ordering::SeqCst);
        let id = resource.id.clone();
        Box::pin(async move { Ok(State::existing(id, HashMap::new())) })
    }

    fn create(
        &self,
        _id: &ResourceId,
        _request: carina_core::provider::CreateRequest,
        attempt: carina_core::provider::CreateAttempt,
    ) -> BoxFuture<'_, Result<CreateOutcome, carina_core::provider::CreateFailure>> {
        Box::pin(async move {
            Ok(attempt
                .issued(
                    AppliedIdentifier::new("mock-id").expect("fixture identifier is non-empty"),
                    Sent::AllRequestAttributes,
                )
                .succeeded(HashMap::new()))
        })
    }

    unexpected_resume_create!();

    fn update(
        &self,
        id: &ResourceId,
        identifier: &carina_core::provider::AppliedIdentifier,
        _request: UpdateRequest,
    ) -> BoxFuture<'_, ProviderResult<UpdateOutcome>> {
        let id = id.clone();
        let identifier = identifier.to_string();
        Box::pin(async move {
            Ok(UpdateOutcome::Success {
                state: AppliedState::new(id, identifier, HashMap::new())
                    .expect("provider input identifier is non-empty"),
            })
        })
    }

    fn delete(
        &self,
        _id: &ResourceId,
        _identifier: &carina_core::provider::AppliedIdentifier,
        _request: DeleteRequest,
    ) -> BoxFuture<'_, ProviderResult<()>> {
        Box::pin(async move { Ok(()) })
    }

    fn required_permissions(&self, _id: &ResourceId, _op: PlanOp) -> ProviderResult<Vec<String>> {
        Ok(Vec::new())
    }
}

struct ManagedAndDataSourceFactory;

struct SavedAttrsHydratingNormalizer;

impl ProviderNormalizer for SavedAttrsHydratingNormalizer {
    fn normalize_desired<'a>(
        &'a self,
        _resources: &'a mut [carina_core::resource::ProviderResource],
    ) -> BoxFuture<'a, ProviderResult<()>> {
        carina_core::provider::ready_noop()
    }

    fn normalize_state<'a>(
        &'a self,
        _states: &'a mut carina_core::provider::StateTransformMap,
    ) -> BoxFuture<'a, ProviderResult<()>> {
        carina_core::provider::ready_noop()
    }

    fn hydrate_read_state<'a>(
        &'a self,
        states: &'a mut carina_core::provider::StateTransformMap,
        saved_attrs: &'a carina_core::provider::SavedAttrs,
    ) -> BoxFuture<'a, ProviderResult<()>> {
        for (id, state) in states {
            if let Some(saved) = saved_attrs.get(id) {
                for (attribute, value) in saved {
                    state
                        .attributes
                        .entry(attribute.clone())
                        .or_insert_with(|| value.clone());
                }
            }
        }
        carina_core::provider::ready_noop()
    }
}

impl ProviderFactory for ManagedAndDataSourceFactory {
    fn name(&self) -> &str {
        "test"
    }

    fn display_name(&self) -> &str {
        "managed/data-source collision test provider"
    }

    fn provider_config_attribute_types(&self) -> carina_core::schema::ValidatedConfigSchema {
        Default::default()
    }

    fn validate_config(
        &self,
        _attributes: &carina_core::provider::CanonicalProviderConfigAttributes,
    ) -> carina_core::provider::ProviderResult<()> {
        Ok(())
    }

    fn extract_region(&self, _attributes: &IndexMap<String, Value>) -> String {
        "local".to_string()
    }

    fn create_provider(
        &self,
        _binding: Option<&str>,
        _config: &carina_core::provider::ProviderReadyConfig,
    ) -> BoxFuture<'_, ProviderResult<Box<dyn Provider>>> {
        Box::pin(async {
            Ok(Box::new(ReadWithRetryProvider::new(ReadBehavior::Success)) as Box<dyn Provider>)
        })
    }

    fn create_normalizer(
        &self,
        _binding: Option<&str>,
        _config: &carina_core::provider::ProviderReadyConfig,
    ) -> BoxFuture<'_, ProviderResult<Box<dyn ProviderNormalizer>>> {
        Box::pin(async {
            Ok(Box::new(SavedAttrsHydratingNormalizer) as Box<dyn ProviderNormalizer>)
        })
    }

    fn schemas(&self) -> Vec<carina_core::schema::ValidatedResourceSchema> {
        let schema_drafts: Vec<carina_core::schema::ResourceSchema> = {
            vec![
                ResourceSchema::new("shared.Type"),
                ResourceSchema::new("shared.Type").as_data_source(),
            ]
        };
        schema_drafts
            .into_iter()
            .map(|schema| {
                schema
                    .validate()
                    .expect("factory test schema must be valid")
            })
            .collect()
    }
}

#[tokio::test]
async fn plan_deletes_managed_orphan_and_reads_same_id_data_source() {
    use carina_core::parser::ProviderContext;
    use carina_state::state::ResourceState;

    let parsed = parse(
        r#"
provider test {}

let shared = read test.shared.Type {}
"#,
        &ProviderContext::default(),
    )
    .expect("parse managed-to-data-source fixture");
    let data_source_id = parsed.data_sources[0].id.clone();
    let identity = data_source_id
        .identity()
        .expect("let-bound data source identity is resolved")
        .clone();
    let mut prior_row = ResourceState::try_applied(
        data_source_id.resource_type.clone(),
        identity,
        data_source_id.provider.clone(),
        "managed-provider-id",
    )
    .expect("fixture identifier is non-empty");
    prior_row.binding = Some("shared".to_string());
    prior_row.set_persisted_attribute(
        "managed_saved",
        serde_json::json!("managed-write-only-value"),
    );
    let partial_read = carina_core::resource::PartialReadMarker::new(
        carina_core::resource::PendingReRead::incomplete("managed read omitted an attribute"),
        BTreeSet::from(["managed_missing".to_string()]),
    )
    .expect("fixture has a missing attribute");
    prior_row = crate::fixture_resource_state_metadata(
        prior_row,
        Some(vec!["managed_saved".to_string()]),
        Some(Some(partial_read.clone())),
    );
    let mut state_file = StateFile::new();
    state_file
        .upsert_resource(prior_row)
        .expect("test state row is unique");
    let state_file = Some(state_file);
    let provider_configs = resolved_provider_configs(&parsed);
    let wiring = ValidatedWiring::from_factories_for_test(
        vec![Box::new(ManagedAndDataSourceFactory)],
        provider_configs.as_provider_configs(),
    );
    let ctx = wiring.context();
    let canonical_resources = canonical_pair(
        &parsed,
        parsed.resources.clone(), // allow: direct — plan-time reconciliation
        ctx.schemas(),
    );
    let validated = ValidatedConfig::from_wiring_for_test(wiring, canonical_resources);

    let plan_context = create_plan_from_parsed_with_upstream_with_ctx(
        validated,
        &provider_configs,
        &state_file,
        true,
        ProgressSink::Human,
        &HashMap::new(),
        &StateBlockClaims::default(),
        &ResolvedStateBlockTargets::default(),
    )
    .await
    .expect("managed orphan and same-id data source must coexist through planning");
    let plan_context = plan_context.resources();

    let summary = plan_context.plan.summary();
    assert_eq!(summary.read, 1);
    assert_eq!(summary.delete, 1);
    assert_eq!(plan_context.plan.effects().len(), 2);
    assert!(plan_context.plan.errors().is_empty());

    let orphan_state = match &plan_context.managed_current[&data_source_id] {
        Current::Present(present) => present.state(),
        Current::Absent(absent) => panic!("managed orphan unexpectedly absent: {absent:?}"),
        Current::MissingIdentifier(unidentified) => {
            panic!("managed orphan unexpectedly lacks an identifier: {unidentified:?}")
        }
        Current::CreateInFlight(pending) => {
            panic!("managed orphan unexpectedly has an in-flight create: {pending:?}")
        }
    };
    assert_eq!(
        orphan_state.attributes.get("managed_saved"),
        Some(&Value::Concrete(ConcreteValue::String(
            "managed-write-only-value".into()
        )))
    );
    assert_eq!(orphan_state.partial_read, Some(partial_read));

    let data_source_state = plan_context
        .data_source_values
        .read_states()
        .find_map(|(id, state)| (id == &data_source_id).then_some(state))
        .expect("plan data source was read");
    assert!(!data_source_state.attributes.contains_key("managed_saved"));
    assert_eq!(data_source_state.partial_read, None);
}

#[tokio::test]
async fn read_with_retry_translates_not_found_to_state_not_found() {
    let provider = ReadWithRetryProvider::new(ReadBehavior::NotFound);
    let id = ResourceId::with_identity("test.Resource", "gone");

    let identifier = crate::fixture_applied_identifier("some-identifier");
    let state = read_with_retry(&provider, &id, &identifier)
        .await
        .expect("not found should translate into a not_found state");

    assert!(!state.exists);
    assert_eq!(state.id, id);
    assert_eq!(state.identifier, None);
}

#[tokio::test]
async fn read_with_retry_propagates_other_errors() {
    let provider = ReadWithRetryProvider::new(ReadBehavior::ApiError);
    let id = ResourceId::with_identity("test.Resource", "broken");

    let identifier = crate::fixture_applied_identifier("some-identifier");
    let result = read_with_retry(&provider, &id, &identifier).await;

    assert!(matches!(result, Err(ProviderError::ApiError(_))));
}

#[tokio::test]
async fn read_with_retry_does_not_retry_not_found() {
    let provider = ReadWithRetryProvider::new(ReadBehavior::NotFoundThenSuccess);
    let id = ResourceId::with_identity("test.Resource", "gone");

    let identifier = crate::fixture_applied_identifier("some-identifier");
    let state = read_with_retry(&provider, &id, &identifier)
        .await
        .expect("not found should translate into a not_found state");

    assert!(!state.exists);
    assert_eq!(state.id, id);
    assert_eq!(state.identifier, None);
    assert_eq!(provider.read_calls(), 1);
}

use carina_core::parser::ParsedFile;

/// Verify that provider state normalization plus host schema lifting prevents
/// false diffs for enum values in state.
///
/// When state contains raw AWS enum values (e.g., "default") and desired
/// resources have been resolved by the host to canonical API values, the
/// differ must compare the same typed representation on both sides.
///
/// Both the plan path (wiring.rs) and the apply path (apply.rs) run provider
/// state normalization before the host's schema-positioned lift.
#[test]
#[ignore = "requires provider binary for state normalization"]
fn test_normalize_state_prevents_false_enum_diff() {
    use carina_core::resource::Directives;

    let ctx = WiringContext::new_for_test(vec![], &[]);

    // Desired resource with the parser-authored typed enum value.
    let mut resource = Resource::with_provider("awscc", "ec2.Vpc", "test-vpc", None);
    resource.set_attr(
        "instance_tenancy".to_string(),
        Value::Concrete(ConcreteValue::enum_identifier(
            "awscc.ec2.Vpc.InstanceTenancy.default",
        )),
    );
    carina_core::value::canonicalize_resources_with_schemas(
        std::slice::from_mut(&mut resource),
        ctx.schemas(),
    )
    .unwrap();

    // State with raw AWS value (as returned by provider.read())
    let id = resource.id.clone();
    let mut state_attrs = HashMap::new();
    state_attrs.insert(
        "instance_tenancy".to_string(),
        Value::Concrete(ConcreteValue::String("default".into())),
    );
    let state = State::existing(id.clone(), state_attrs).with_identifier("test-vpc");
    let mut current_states = HashMap::new();
    current_states.insert(id.clone(), Current::from_state(state));

    // Without normalize_state, the differ would see a false diff
    let resources_without = vec![resource.clone()];
    let directives_map: HashMap<ResourceId, Directives> = HashMap::new();
    let schemas = SchemaRegistry::new();
    let saved_attrs = crate::empty_lifted_saved_attrs();
    let prev_explicit = HashMap::new();
    let orphan_deps = Orphans::new();
    let plan_without = create_plan(
        &resources_without,
        &[],
        &carina_core::provider::ProviderRouter::new(),
        &carina_core::resource::into_plan_input_current_map::<carina_core::resource::ModuleSource>(
            current_states.clone(),
            &carina_core::schema::SchemaRegistry::new(),
            &[],
        ),
        &directives_map,
        &schemas,
        &saved_attrs,
        &prev_explicit,
        &orphan_deps,
        &[],
    );
    assert!(
        !plan_without.effects().is_empty(),
        "Without normalize_state, differ should see a false diff"
    );

    // After normalize_state, state values match desired values → no diff
    let provider_configs = validated_provider_configs(Vec::new());
    normalize_state_with_ctx(&ctx, &provider_configs, &mut current_states).unwrap();
    let resources_with = vec![resource];
    let plan_with = create_plan(
        &resources_with,
        &[],
        &carina_core::provider::ProviderRouter::new(),
        &carina_core::resource::into_plan_input_current_map::<carina_core::resource::ModuleSource>(
            current_states.clone(),
            &carina_core::schema::SchemaRegistry::new(),
            &[],
        ),
        &directives_map,
        &schemas,
        &saved_attrs,
        &prev_explicit,
        &orphan_deps,
        &[],
    );
    assert!(
        plan_with.effects().is_empty(),
        "After normalize_state, no false diff should occur"
    );
}

#[test]
fn normalize_state_with_ctx_prepares_config_inside_time_enabled_multithread_runtime() {
    struct BlockInPlaceValidationFactory {
        validated: Arc<AtomicBool>,
        received_region: Arc<Mutex<Option<Value>>>,
    }

    impl ProviderFactory for BlockInPlaceValidationFactory {
        fn name(&self) -> &str {
            "block-in-place"
        }

        fn display_name(&self) -> &str {
            "block-in-place validation test provider"
        }

        fn provider_config_attribute_types(&self) -> carina_core::schema::ValidatedConfigSchema {
            Default::default()
        }

        fn validate_config(
            &self,
            attributes: &carina_core::provider::CanonicalProviderConfigAttributes,
        ) -> carina_core::provider::ProviderResult<()> {
            tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current()
                    .block_on(tokio::time::sleep(std::time::Duration::ZERO));
            });
            self.validated.store(true, Ordering::SeqCst);
            *self.received_region.lock().unwrap() = attributes.get("region").cloned();
            Ok(())
        }

        fn extract_region(&self, _attributes: &IndexMap<String, Value>) -> String {
            "test-region".to_string()
        }

        fn create_provider(
            &self,
            _binding: Option<&str>,
            _config: &carina_core::provider::ProviderReadyConfig,
        ) -> BoxFuture<'_, ProviderResult<Box<dyn Provider>>> {
            Box::pin(async { unreachable!("state normalization does not create providers") })
        }

        fn schemas(&self) -> Vec<carina_core::schema::ValidatedResourceSchema> {
            let schema_drafts: Vec<carina_core::schema::ResourceSchema> = Vec::new();
            schema_drafts
                .into_iter()
                .map(|schema| {
                    schema
                        .validate()
                        .expect("factory test schema must be valid")
                })
                .collect()
        }
    }

    let validated = Arc::new(AtomicBool::new(false));
    let received_region = Arc::new(Mutex::new(None));
    let raw_configs = vec![ProviderConfig {
        name: "block-in-place".to_string(),
        attributes: IndexMap::from([(
            "region".to_string(),
            Value::Concrete(ConcreteValue::String("resolved-region".into())),
        )]),
        default_tags: IndexMap::new(),
        source: None,
        version: None,
        revision: None,
        unresolved_attributes: IndexMap::new(),
        binding: None,
    }];
    let provider_configs = validated_provider_configs(raw_configs);
    let ctx = WiringContext::new_for_test(
        vec![Box::new(BlockInPlaceValidationFactory {
            validated: Arc::clone(&validated),
            received_region: Arc::clone(&received_region),
        })],
        provider_configs.as_provider_configs(),
    );

    normalize_state_with_ctx(&ctx, &provider_configs, &mut HashMap::new())
        .expect("provider validation should run inside a time-enabled compatible runtime");

    assert!(validated.load(Ordering::SeqCst));
    assert_eq!(
        *received_region.lock().unwrap(),
        Some(Value::Concrete(ConcreteValue::String(
            "resolved-region".into()
        ))),
        "state normalization must prepare the declared provider config"
    );
}

#[test]
fn normalize_state_with_ctx_renders_provider_config_error_once() {
    let (_parsed, provider_configs) = resolved_invalid_provider_config();
    let ctx = WiringContext::new_for_test(
        vec![Box::new(ProviderConfigConstraintFactory {
            creates: Arc::new(AtomicUsize::new(0)),
        })],
        provider_configs.as_provider_configs(),
    );

    let error = normalize_state_with_ctx(&ctx, &provider_configs, &mut HashMap::new())
        .expect_err("invalid provider configuration must fail state normalization setup");
    let preparation_error = error
        .detail()
        .cause()
        .expect("configuration preparation error must remain the typed source");
    let leaf_reason = preparation_error.to_string();
    let rendered = error.to_string();

    assert_eq!(
        rendered.matches(&leaf_reason).count(),
        1,
        "leaf TypeError must render once, got: {rendered}"
    );
}

#[test]
fn state_bucket_config_prefers_default_then_first_named_then_implicit_empty() {
    struct CaptureFactory;

    impl ProviderFactory for CaptureFactory {
        fn name(&self) -> &str {
            "capture"
        }

        fn display_name(&self) -> &str {
            "capture"
        }

        fn provider_config_attribute_types(&self) -> carina_core::schema::ValidatedConfigSchema {
            Default::default()
        }

        fn validate_config(
            &self,
            _attributes: &carina_core::provider::CanonicalProviderConfigAttributes,
        ) -> carina_core::provider::ProviderResult<()> {
            Ok(())
        }

        fn extract_region(&self, _attributes: &IndexMap<String, Value>) -> String {
            "test".to_string()
        }

        fn create_provider(
            &self,
            _binding: Option<&str>,
            _config: &carina_core::provider::ProviderReadyConfig,
        ) -> BoxFuture<'_, ProviderResult<Box<dyn Provider>>> {
            unreachable!("config selection test does not construct a provider")
        }

        fn schemas(&self) -> Vec<carina_core::schema::ValidatedResourceSchema> {
            let schema_drafts: Vec<carina_core::schema::ResourceSchema> = Vec::new();
            schema_drafts
                .into_iter()
                .map(|schema| {
                    schema
                        .validate()
                        .expect("factory test schema must be valid")
                })
                .collect()
        }
    }

    let config = |region: &str, binding: Option<&str>| ProviderConfig {
        name: "capture".to_string(),
        attributes: IndexMap::from([(
            "region".to_string(),
            Value::Concrete(ConcreteValue::String(region.to_string().into())),
        )]),
        default_tags: IndexMap::new(),
        source: None,
        version: None,
        revision: None,
        unresolved_attributes: IndexMap::new(),
        binding: binding.map(str::to_string),
    };

    let configs = validated_provider_configs(vec![
        config("first-named-region", Some("main")),
        config("second-named-region", Some("secondary")),
        config("default-region", None),
    ]);
    let ready = prepare_default_provider_ready_config(&CaptureFactory, "capture", &configs)
        .expect("the default provider instance must win");
    assert_eq!(
        ready.attributes().get("region"),
        Some(&Value::Concrete(ConcreteValue::String(
            "default-region".into()
        )))
    );

    let configs = validated_provider_configs(vec![
        config("first-named-region", Some("main")),
        config("second-named-region", Some("secondary")),
    ]);
    let ready = prepare_default_provider_ready_config(&CaptureFactory, "capture", &configs)
        .expect("the first named provider instance must supply backend config");
    assert_eq!(
        ready.attributes().get("region"),
        Some(&Value::Concrete(ConcreteValue::String(
            "first-named-region".into()
        )))
    );

    let configs = validated_provider_configs(Vec::new());
    let ready = prepare_default_provider_ready_config(&CaptureFactory, "capture", &configs)
        .expect("an undeclared provider kind must retain the empty fallback");
    assert!(ready.attributes().is_empty());
}

/// Verify that host default-tag merging prevents false diffs when default_tags are
/// configured in the provider block.
///
/// When default_tags are set and state already contains those tags (from a
/// previous apply), the differ must not report a diff for the tags. This
/// requires the desired normalization pipeline to add the default tags before
/// diffing.
///
/// Both the plan path and the apply path use the shared desired normalization
/// pipeline to maintain parity.
#[test]
fn test_merge_default_tags_prevents_false_diff() {
    use carina_core::resource::Directives;
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};

    // Build a minimal schema with the map(string) `tags` attribute required by
    // host default-tag merging.
    let schema = ResourceSchema::new("s3.Bucket").attribute(
        AttributeSchema::optional("tags", AttributeType::map(AttributeType::string())).build(),
    );
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        schema.validate().expect("schema fixture must be valid"),
    );

    // Desired resource without explicit tags
    let resource = Resource::with_provider("awscc", "s3.Bucket", "test-bucket", None);

    // State already has the default tags (from a previous apply)
    let id = resource.id.clone();
    let mut state_attrs = HashMap::new();
    let mut tags = IndexMap::new();
    tags.insert(
        "Environment".to_string(),
        Value::Concrete(ConcreteValue::String("production".into())),
    );
    state_attrs.insert(
        "tags".to_string(),
        Value::Concrete(ConcreteValue::Map(tags)),
    );
    let state = State::existing(id.clone(), state_attrs).with_identifier("test-bucket");
    let mut current_states = HashMap::new();
    current_states.insert(id.clone(), state);

    let default_tags: IndexMap<String, Value> = {
        let mut m = IndexMap::new();
        m.insert(
            "Environment".to_string(),
            Value::Concrete(ConcreteValue::String("production".into())),
        );
        m
    };

    // Simulate prev_explicit from a previous apply that included "tags"
    // (because default tags were merged correctly in the plan path).
    let mut prev_explicit: HashMap<ResourceId, carina_core::explicit::ExplicitFields> =
        HashMap::new();
    prev_explicit.insert(
        resource.id.clone(),
        carina_core::explicit::ExplicitFields::Struct {
            children: std::collections::HashMap::from([(
                "tags".to_string(),
                carina_core::explicit::ExplicitFields::Leaf,
            )]),
        },
    );

    // Without host merging, the desired resource has no tags,
    // but state has tags and prev_explicit says "tags" was previously desired.
    // The differ sees this as attribute removal → false Update diff.
    let resources_without = vec![resource.clone()];
    let directives_map: HashMap<ResourceId, Directives> = HashMap::new();
    let saved_attrs = crate::empty_lifted_saved_attrs();
    let orphan_deps = Orphans::new();
    let plan_without = create_plan(
        &resources_without,
        &[],
        &carina_core::provider::ProviderRouter::new(),
        &crate::plan_input_current_from_states(
            current_states.clone(),
            &carina_core::schema::SchemaRegistry::new(),
            &[],
        ),
        &directives_map,
        &schemas,
        &saved_attrs,
        &prev_explicit,
        &orphan_deps,
        &[],
    );
    assert!(
        !plan_without.effects().is_empty(),
        "without host default-tag merging, differ should see a false diff"
    );

    // After host merging, desired resource gains the default tags → no diff.
    let config = ProviderConfig {
        name: "awscc".to_string(),
        attributes: IndexMap::new(),
        default_tags,
        source: None,
        version: None,
        revision: None,
        unresolved_attributes: IndexMap::new(),
        binding: None,
    };
    let mut resources_with = vec![resource];
    carina_core::provider::merge_default_tags(&config, &mut resources_with, &schemas).unwrap();

    // After merging, desired now has tags matching state → no diff
    let plan_with = create_plan(
        &resources_with,
        &[],
        &carina_core::provider::ProviderRouter::new(),
        &crate::plan_input_current_from_states(
            current_states.clone(),
            &carina_core::schema::SchemaRegistry::new(),
            &[],
        ),
        &directives_map,
        &schemas,
        &saved_attrs,
        &prev_explicit,
        &orphan_deps,
        &[],
    );
    assert!(
        plan_with.effects().is_empty(),
        "after host default-tag merging, no false diff should occur"
    );
}

/// Sibling of the Import/Removed routing fix:
/// `moved { from = X 'a', to = X 'b' }` faces the same address-
/// equality bug when the state-resident `a` carries
/// `provider_instance = Some("X")`. Without the root fix
/// `managed_current.remove(from)` misses (since the key in the map has
/// the routed id), the moved state is never transferred, and the
/// orphan Delete for `a` is never suppressed — the operator sees both
/// `<- moved` and `- delete` for the same address.
///
/// Coverage shape: state has `a` under routed id; `materialize_moved_states`
/// must (a) actually move the entry under `b`'s routed id, and (b)
/// return a `(from, to)` pair whose ids carry the routing so the
/// downstream `suppress_delete` set in `add_state_block_effects`
/// keys correctly.
#[test]
fn moved_block_resolves_routed_instance_on_from_and_to() {
    use carina_core::resource::ResourceId;
    use carina_state::state::{ResourceState, StateFile};

    // State has the source resource under a routed id.
    let mut state_file = StateFile::new();
    let mut rs = ResourceState::new("route53.RecordSet", "old_record", "aws");
    rs.directives.provider_instance = Some("management".to_string());
    state_file
        .upsert_resource(rs)
        .expect("test state setup must be valid");

    // Pre-populate managed current state with the routed `from` id (mirrors
    // what `build_state_for_resource` would produce).
    let routed_from = ResourceId::with_provider_identity(
        "aws",
        "route53.RecordSet",
        "old_record",
        Some("management".to_string()),
    );
    let mut current_states = HashMap::new();
    current_states.insert(routed_from.clone(), Current::absent(routed_from.clone()));

    let mut prev_explicit = HashMap::new();
    let mut saved_attrs = crate::empty_lifted_saved_attrs();

    // `moved` block addresses are routing-agnostic by construction
    // (the type makes routing unrepresentable here).
    let state_blocks = vec![StateBlock::Moved {
        addresses: MovedAddresses::new("aws", "route53.RecordSet", "old_record", "new_record"),
    }];

    let moved_pairs = materialize_moved_states(
        &mut current_states,
        &mut Orphans::new(),
        &mut prev_explicit,
        &mut saved_attrs,
        &state_blocks,
        &Some(state_file),
    )
    .expect("fixture move routes must agree with current state");

    // The pair must inherit `from`'s routing so the downstream
    // `suppress_delete` lookup matches the orphan Delete's id.
    assert_eq!(moved_pairs.len(), 1);
    let (from, _to) = &moved_pairs[0];
    assert_eq!(
        from.provider_instance.as_deref(),
        Some("management"),
        "moved.from must inherit routing from the matched state row, got {from:?}",
    );

    // And managed current state must no longer carry the old key — the
    // state was actually transferred (not silently left under the
    // routed id while a None-routing key was inserted).
    assert!(
        !current_states.contains_key(&routed_from),
        "old routed key must be removed after move",
    );
}

#[test]
fn same_type_moved_block_parses_and_materializes_state() {
    use carina_core::parser::ProviderContext;
    use carina_core::resource::ResourceId;
    use carina_state::state::{ResourceState, StateFile};

    let parsed = parse(
        r#"
            moved {
                from = awscc.s3.Bucket 'old_bucket'
                to   = awscc.s3.Bucket 'new_bucket'
            }
        "#,
        &ProviderContext::default(),
    )
    .expect("same-type moved block must parse");

    let from = ResourceId::with_provider_identity("awscc", "s3.Bucket", "old_bucket", None);
    let to = ResourceId::with_provider_identity("awscc", "s3.Bucket", "new_bucket", None);
    let mut current_states = HashMap::from([(to.clone(), Current::absent(to.clone()))]);
    let mut state_file = StateFile::new();
    state_file
        .upsert_resource(
            ResourceState::try_applied("s3.Bucket", "old_bucket", "awscc", "bucket-123")
                .expect("fixture identifier is non-empty"),
        )
        .expect("test state setup must be valid");
    let mut orphans = state_file
        .build_orphans(&HashSet::from([to.clone()]))
        .into_plan_orphans();

    let moved_pairs = materialize_moved_states(
        &mut current_states,
        &mut orphans,
        &mut HashMap::new(),
        &mut crate::empty_lifted_saved_attrs(),
        &parsed.state_blocks,
        &Some(state_file),
    )
    .expect("fixture move routes must agree with current state");

    assert_eq!(
        moved_pairs,
        vec![(
            ResolvedResourceId::new(from.clone()),
            ResolvedResourceId::new(to.clone())
        )]
    );
    assert!(!current_states.contains_key(&from));
    let moved = current_states
        .get(&to)
        .expect("source state must be transferred to the destination address");
    let moved = match moved {
        Current::Absent(state) => state.state(),
        Current::Present(present) => present.state(),
        Current::MissingIdentifier(state) => state.state(),
        Current::CreateInFlight(pending) => {
            panic!("moved test state unexpectedly remained in flight: {pending:?}")
        }
    };
    assert_eq!(moved.id, to);
    assert_eq!(moved.identifier.as_deref(), Some("bucket-123"));
    assert!(orphans.is_empty());
}

#[test]
fn moved_in_flight_rebinds_resume_to_destination() {
    use carina_core::effect::Effect;
    use carina_core::parser::ProviderContext;
    use carina_core::provider::Sent;
    use carina_state::state::{ResourceState, StateFile};

    let parsed = parse(
        r#"
            moved {
                from = mock.test.resource 'old_name'
                to   = mock.test.resource 'new_name'
            }
        "#,
        &ProviderContext::default(),
    )
    .expect("same-type moved block must parse");
    let from = ResourceId::with_provider_identity("mock", "test.resource", "old_name", None);
    let to = ResourceId::with_provider_identity("mock", "test.resource", "new_name", None);
    let desired = Resource::with_provider("mock", "test.resource", "new_name", None);
    let old_resource = ResolvedResource::new(Resource::with_provider(
        "mock",
        "test.resource",
        "old_name",
        None,
    ));
    let pending =
        crate::fixture_in_flight_create(&from, "opaque-token", None, Sent::AllRequestAttributes);
    let row = ResourceState::from_in_flight(&old_resource, &pending, None, None, None)
        .expect("in-flight fixture must encode");
    let mut state_file = StateFile::new();
    state_file
        .upsert_resource(row)
        .expect("test state setup must be valid");
    let mut current_states = HashMap::from([(to.clone(), Current::absent(to.clone()))]);
    let mut orphans = state_file
        .build_orphans(&HashSet::from([to.clone()]))
        .into_plan_orphans();

    let moved_pairs = materialize_moved_states(
        &mut current_states,
        &mut orphans,
        &mut HashMap::new(),
        &mut crate::empty_lifted_saved_attrs(),
        &parsed.state_blocks,
        &Some(state_file),
    )
    .expect("fixture move routes must agree with current state");

    assert_eq!(
        moved_pairs,
        vec![(
            ResolvedResourceId::new(from),
            ResolvedResourceId::new(to.clone())
        )]
    );
    let Current::CreateInFlight(rebound) = &current_states[&to] else {
        panic!("moved continuation must retain its in-flight type");
    };
    assert_eq!(rebound.id(), &to);
    let plan_current = carina_core::resource::into_plan_input_current_map(
        current_states,
        &SchemaRegistry::new(),
        std::slice::from_ref(&desired),
    );
    let plan = create_plan(
        std::slice::from_ref(&desired),
        &[],
        &carina_core::provider::ProviderRouter::new(),
        &plan_current,
        &HashMap::new(),
        &SchemaRegistry::new(),
        &crate::empty_lifted_saved_attrs(),
        &HashMap::new(),
        &orphans,
        &[],
    );
    assert!(matches!(
        plan.effects(),
        [Effect::ResumeCreate { resource, .. }] if resource.id == to
    ));
}

#[test]
fn moved_orphan_to_undeclared_address_keeps_binding_on_delete() {
    use carina_core::effect::Effect;
    use carina_core::parser::ProviderContext;
    use carina_state::state::{ResourceState, StateFile};

    let parsed = parse(
        r#"
            moved {
                from = mock.test.resource 'old_name'
                to   = mock.test.resource 'new_name'
            }
        "#,
        &ProviderContext::default(),
    )
    .expect("same-type moved block must parse");

    let from = ResourceId::with_provider_identity("mock", "test.resource", "old_name", None);
    let to = ResourceId::with_provider_identity("mock", "test.resource", "new_name", None);
    let mut row = ResourceState::try_applied("test.resource", "old_name", "mock", "object-123")
        .expect("fixture identifier is non-empty");
    row.binding = Some("retired".to_string());
    let mut state_file = StateFile::new();
    state_file.upsert_resource(row).unwrap();

    let mut orphans = state_file
        .build_orphans(&HashSet::new())
        .into_plan_orphans();
    let mut current_states = HashMap::new();
    let moved_pairs = materialize_moved_states(
        &mut current_states,
        &mut orphans,
        &mut HashMap::new(),
        &mut crate::empty_lifted_saved_attrs(),
        &parsed.state_blocks,
        &Some(state_file.clone()),
    )
    .expect("fixture move routes must agree with current state");
    assert_eq!(
        moved_pairs,
        vec![(
            ResolvedResourceId::new(from),
            ResolvedResourceId::new(to.clone())
        )]
    );
    assert!(
        current_states.is_empty(),
        "an undeclared move target must remain solely in the orphan collection"
    );
    assert_eq!(
        orphans.get(&to).and_then(|orphan| orphan.binding()),
        Some("retired")
    );

    let plan =
        create_plan(
            &[],
            &[],
            &carina_core::provider::ProviderRouter::new(),
            &carina_core::resource::into_plan_input_current_map::<
                carina_core::resource::ModuleSource,
            >(current_states, &SchemaRegistry::new(), &[]),
            &state_file.build_directives(),
            &SchemaRegistry::new(),
            &crate::empty_lifted_saved_attrs(),
            &HashMap::new(),
            &orphans,
            &[],
        );

    assert!(matches!(
        plan.effects(),
        [Effect::Delete { id, binding, .. }]
            if id.as_inner() == &to && binding.as_deref() == Some("retired")
    ));
}

/// Sibling of `import_suppresses_create_when_target_resource_is_routed_to_named_instance`:
/// `removed { from = X 'addr' }` faces the same address-equality
/// mismatch when the state-resident resource has
/// `provider_instance = Some("X")` (originally created via
/// `directives { provider = X }`) but the user-typed `from` parses as
/// `provider_instance = None`. Without the root fix the orphan Delete
/// stays in the plan and the operator sees both `<- removed` and
/// `- delete` for the same address (carina#3324 root cause class).
#[test]
fn removed_block_suppresses_delete_when_state_resource_is_routed_to_named_instance() {
    use carina_core::effect::Effect;
    use carina_core::plan::Plan;
    use carina_core::resource::ResourceId;
    use carina_state::state::{ResourceState, StateFile};

    // State has a resource that was originally created via
    // `directives { provider = management }` — `provider_instance =
    // Some("management")`.
    let mut state_file = StateFile::new();
    let mut rs = ResourceState::new("route53.RecordSet", "r.delegation_ns", "aws");
    rs.directives.provider_instance = Some("management".to_string());
    state_file
        .upsert_resource(rs)
        .expect("test state setup must be valid");

    // The orphan Delete effect carries the same routed instance.
    let mut plan = Plan::new();
    plan.add(Effect::Delete {
        id: carina_core::resource::ResolvedResourceId::new(ResourceId::with_provider_identity(
            "aws",
            "route53.RecordSet",
            "r.delegation_ns",
            Some("management".to_string()),
        )),
        identifier: crate::fixture_applied_identifier("record-current"),
        generation: carina_core::effect::EffectGeneration::Current,
        directives: carina_core::resource::Directives::default(),
        binding: None,
        dependencies: std::collections::HashSet::new(),
        explicit_dependencies: std::collections::HashSet::new(),
        blocked_by_updates: std::collections::HashSet::new(),
    });

    // `removed` block addresses are routing-agnostic by construction
    // — the newtype makes routing unrepresentable here.
    let state_blocks = vec![StateBlock::Removed {
        from: StateBlockAddress::new("aws", "route53.RecordSet", "r.delegation_ns"),
    }];

    let bindings = carina_core::binding_index::ResolvedBindings::default();
    let no_upstreams: std::collections::HashSet<&str> = std::collections::HashSet::new();
    add_state_block_effects(
        &mut plan,
        &state_blocks,
        &Some(state_file),
        &[],
        &[],
        &bindings,
        &no_upstreams,
    );

    let effects = plan.effects();
    assert_eq!(
        effects.len(),
        1,
        "expected only the Remove effect (orphan Delete must be suppressed \
         when the same address is being removed), got {effects:?}",
    );
    match &effects[0] {
        Effect::Remove { id } => {
            assert_eq!(
                id.provider_instance.as_deref(),
                Some("management"),
                "the remove effect must inherit the routed instance from the \
                 state row so apply-time routing sends the state-removal call \
                 to the correct provider instance",
            );
        }
        other => panic!("expected Remove effect, got {other:?}"),
    }
}

#[test]
fn removed_block_does_not_suppress_deposed_generation_delete() {
    use carina_core::effect::{Effect, EffectGeneration};
    use carina_core::plan::Plan;
    use carina_core::resource::ResourceId;
    use carina_state::state::{DeposedInstance, DeposedKey, ResourceState, StateFile};
    use std::collections::{BTreeSet, HashMap};

    let deposed_key = DeposedKey::new_unique();
    let mut state_file = StateFile::new();
    let mut rs = ResourceState::try_applied(
        "route53.RecordSet",
        "r.delegation_ns",
        "aws",
        "record-current",
    )
    .expect("fixture identifier is non-empty");
    rs.deposed.push(DeposedInstance::from_persisted_json(
        deposed_key.clone(),
        "record-deposed".to_string(),
        None,
        HashMap::new(),
        BTreeSet::new(),
    ));
    state_file
        .upsert_resource(rs)
        .expect("test state setup must be valid");

    let id =
        ResourceId::with_provider_identity("aws", "route53.RecordSet", "r.delegation_ns", None);
    let mut plan = Plan::new();
    plan.add(Effect::Delete {
        id: carina_core::resource::ResolvedResourceId::new(id.clone()),
        identifier: crate::fixture_applied_identifier("record-current"),
        generation: EffectGeneration::Current,
        directives: carina_core::resource::Directives::default(),
        binding: None,
        dependencies: std::collections::HashSet::new(),
        explicit_dependencies: std::collections::HashSet::new(),
        blocked_by_updates: std::collections::HashSet::new(),
    });
    add_deposed_delete_effects(&mut plan, &Some(state_file.clone()), &[]);

    let state_blocks = vec![StateBlock::Removed {
        from: StateBlockAddress::new("aws", "route53.RecordSet", "r.delegation_ns"),
    }];
    let bindings = carina_core::binding_index::ResolvedBindings::default();
    let no_upstreams: std::collections::HashSet<&str> = std::collections::HashSet::new();
    add_state_block_effects(
        &mut plan,
        &state_blocks,
        &Some(state_file),
        &[],
        &[],
        &bindings,
        &no_upstreams,
    );

    assert!(
        !plan.effects().iter().any(|effect| {
            matches!(
                effect,
                Effect::Delete {
                    generation: EffectGeneration::Current,
                    ..
                }
            )
        }),
        "removed block must suppress only the current-instance orphan delete"
    );
    assert!(
        plan.effects().iter().any(|effect| {
            matches!(
                effect,
                Effect::Delete {
                    identifier,
                    generation: EffectGeneration::Deposed(key),
                    ..
                } if identifier.as_str() == "record-deposed" && key == &deposed_key
            )
        }),
        "deposed generation delete must stay in the plan"
    );
    assert!(
        plan.effects()
            .iter()
            .any(|effect| matches!(effect, Effect::Remove { .. })),
        "removed block must still emit the Remove state operation"
    );
}

#[test]
fn removed_suppresses_in_flight_orphan_error_and_keeps_deposed() {
    use carina_core::effect::{Effect, EffectGeneration};
    use carina_core::provider::Sent;
    use carina_core::resource::{ResolvedResource, Resource};
    use carina_state::state::{DeposedInstance, DeposedKey, ResourceState, StateFile};
    use std::collections::{BTreeSet, HashMap, HashSet};

    let resource = ResolvedResource::new(Resource::with_provider(
        "mock",
        "example.Widget",
        "pending",
        None,
    ));
    let pending = crate::fixture_in_flight_create(
        &resource.id,
        "opaque-token",
        None,
        Sent::AllRequestAttributes,
    );
    let mut row = ResourceState::from_in_flight(&resource, &pending, None, None, None)
        .expect("in-flight fixture must encode");
    let deposed_key = DeposedKey::new_unique();
    row.deposed.push(DeposedInstance::from_persisted_json(
        deposed_key.clone(),
        "old-id".to_string(),
        None,
        HashMap::new(),
        BTreeSet::new(),
    ));
    let mut state_file = StateFile::new();
    state_file
        .upsert_resource(row)
        .expect("test state setup must be valid");

    let orphans = state_file
        .build_orphans(&HashSet::new())
        .into_plan_orphans();
    let mut plan = create_plan(
        &[],
        &[],
        &carina_core::provider::ProviderRouter::new(),
        &HashMap::new(),
        &state_file.build_directives(),
        &SchemaRegistry::new(),
        &crate::empty_lifted_saved_attrs(),
        &HashMap::new(),
        &orphans,
        &[],
    );
    assert!(matches!(
        plan.errors(),
        [carina_core::plan::PlanError {
            kind: carina_core::plan::PlanErrorKind::CreateInFlightOrphan,
            ..
        }]
    ));
    add_deposed_delete_effects(&mut plan, &Some(state_file.clone()), &[]);

    add_state_block_effects(
        &mut plan,
        &[StateBlock::Removed {
            from: StateBlockAddress::new("mock", "example.Widget", "pending"),
        }],
        &Some(state_file),
        &[],
        &[],
        &carina_core::binding_index::ResolvedBindings::default(),
        &HashSet::new(),
    );

    assert!(plan.errors().is_empty());
    assert!(
        plan.effects()
            .iter()
            .any(|effect| matches!(effect, Effect::Remove { id } if id.as_inner() == &resource.id))
    );
    assert!(plan.effects().iter().any(|effect| matches!(
        effect,
        Effect::Delete {
            generation: EffectGeneration::Deposed(key),
            ..
        } if key == &deposed_key
    )));
}

#[test]
fn deposed_delete_without_provider_identifier_becomes_plan_error() {
    use carina_core::plan::{Plan, PlanErrorKind};
    use carina_state::state::{DeposedInstance, DeposedKey, ResourceState, StateFile};
    use std::collections::{BTreeSet, HashMap};

    let mut row = ResourceState::new("example.Widget", "broken", "mock");
    let deposed_key = DeposedKey::new_unique();
    row.deposed.push(DeposedInstance::from_persisted_json(
        deposed_key.clone(),
        String::new(),
        None,
        HashMap::new(),
        BTreeSet::new(),
    ));
    let mut state_file = StateFile::new();
    state_file
        .upsert_resource(row)
        .expect("test state setup must be valid");
    let mut plan = Plan::new();

    add_deposed_delete_effects(&mut plan, &Some(state_file), &[]);

    assert!(plan.effects().is_empty());
    assert_eq!(plan.errors().len(), 1);
    assert_eq!(
        plan.errors()[0].kind,
        PlanErrorKind::MissingDeposedProviderIdentifier {
            key: deposed_key,
            current_identifier: None,
        }
    );
    let message = plan.errors()[0].to_string();
    assert!(message.contains("mock.example.Widget.broken"), "{message}");
    assert!(message.contains("`removed` block"), "{message}");
    assert!(!message.contains("`import` block"), "{message}");
}

#[test]
fn malformed_deposed_generation_with_healthy_current_instance_recommends_reimport() {
    use carina_core::plan::Plan;
    use carina_state::state::{DeposedInstance, DeposedKey, ResourceState, StateFile};
    use std::collections::{BTreeSet, HashMap};

    let mut row =
        ResourceState::try_applied("example.Widget", "healthy-current", "mock", "current-id")
            .expect("fixture identifier is non-empty");
    row.deposed.push(DeposedInstance::from_persisted_json(
        DeposedKey::new_unique(),
        String::new(),
        None,
        HashMap::new(),
        BTreeSet::new(),
    ));
    let mut state_file = StateFile::new();
    state_file
        .upsert_resource(row)
        .expect("test state setup must be valid");
    let mut plan = Plan::new();

    add_deposed_delete_effects(&mut plan, &Some(state_file), &[]);

    let message = plan.errors()[0].to_string();
    assert!(message.contains("`removed` block"), "{message}");
    assert!(message.contains("`import` block"), "{message}");
    assert!(message.contains("current-id"), "{message}");
}

#[test]
fn deposed_delete_waits_for_update_that_dropped_prior_reference() {
    use carina_core::effect::{Effect, EffectGeneration};
    use carina_core::plan::Plan;
    use carina_core::resource::{
        ConcreteValue, ResolvedResource, Resource, ResourceId, State, Value,
    };
    use carina_state::state::{DeposedInstance, DeposedKey, ResourceState, StateFile};
    use std::collections::{BTreeSet, HashMap};

    let producer_id =
        ResourceId::with_provider_identity("aws", "example.Producer", "producer-id", None);
    let consumer_id =
        ResourceId::with_provider_identity("aws", "example.Consumer", "consumer-id", None);
    let mut producer_row = ResourceState::new("example.Producer", "producer-id", "aws");
    producer_row.binding = Some("producer".to_string());
    producer_row
        .deposed
        .push(DeposedInstance::from_persisted_json(
            DeposedKey::new_unique(),
            "producer-old".to_string(),
            None,
            HashMap::new(),
            BTreeSet::new(),
        ));
    let mut state_file = StateFile::new();
    state_file
        .upsert_resource(producer_row)
        .expect("test state setup must be valid");

    let from = State::existing(
        consumer_id.clone(),
        HashMap::from([(
            "version".to_string(),
            Value::Concrete(ConcreteValue::Int(1)),
        )]),
    )
    .with_identifier("consumer-id")
    .with_dependency_bindings(BTreeSet::from(["producer".to_string()]));
    let to = Resource::with_provider("aws", "example.Consumer", "consumer-id", None)
        .with_binding("consumer")
        .with_attribute("version", Value::Concrete(ConcreteValue::Int(2)));
    let mut plan = Plan::new();
    plan.add(Effect::Update {
        identifier: crate::fixture_applied_identifier("consumer-id"),
        from: Box::new(from),
        to: ResolvedResource::new(to),
        changed_attributes: vec!["version".to_string()],
    });

    add_deposed_delete_effects(&mut plan, &Some(state_file.clone()), &[]);
    carina_core::differ::block_deletes_on_prior_consumer_updates(
        &mut plan,
        &state_file.build_directives(),
    );

    let delete = plan
        .effects()
        .iter()
        .find(|effect| {
            matches!(effect, Effect::Delete {
                id,
                generation: EffectGeneration::Deposed(_),
                ..
            } if id.as_inner() == &producer_id)
        })
        .expect("Deposed producer Delete must be added");
    let Effect::Delete {
        blocked_by_updates, ..
    } = delete
    else {
        unreachable!();
    };
    assert_eq!(blocked_by_updates.len(), 1);
    assert!(
        blocked_by_updates
            .iter()
            .any(|identity| identity.as_str() == "consumer-id")
    );
}

#[test]
fn deposed_delete_waits_for_the_rows_in_flight_create() {
    use carina_core::effect::{Effect, EffectGeneration};
    use carina_core::provider::Sent;
    use carina_state::state::{DeposedInstance, DeposedKey, ResourceState};
    use std::collections::{BTreeSet, HashMap};

    let resource = ResolvedResource::new(Resource::with_provider(
        "mock",
        "example.Widget",
        "pending",
        None,
    ));
    let pending =
        crate::fixture_in_flight_create(&resource.id, "opaque", None, Sent::AllRequestAttributes);
    let mut row = ResourceState::from_in_flight(&resource, &pending, None, None, None)
        .expect("in-flight fixture must encode");
    let key = DeposedKey::new_unique();
    row.deposed.push(DeposedInstance::from_persisted_json(
        key.clone(),
        "old-id".to_string(),
        None,
        HashMap::new(),
        BTreeSet::new(),
    ));

    let (effects, errors) = deposed_delete_effects_for_row(&row, &[]);

    assert!(errors.is_empty());
    let [
        Effect::Delete {
            generation: EffectGeneration::Deposed(actual_key),
            blocked_by_updates,
            ..
        },
    ] = effects.as_slice()
    else {
        panic!("expected one deposed delete, got {effects:?}");
    };
    assert_eq!(actual_key, &key);
    assert_eq!(
        blocked_by_updates,
        &std::collections::HashSet::from([resource.identity().clone()])
    );
}

/// carina#3324 regression: an import block targeting a let-bound
/// resource whose `directives { provider = <name> }` routes it to a
/// named provider instance must still suppress the resource's Create
/// effect. Before the fix, the import target was carrying the user-
/// typed `provider_instance = None` while the Create's resource had
/// the directive-routed `provider_instance = Some("management")`, so
/// the `suppress_create` set never matched and the same address
/// appeared as both `<- import` and `+ add` in the plan.
#[test]
fn import_suppresses_create_when_target_resource_is_routed_to_named_instance() {
    use carina_core::effect::Effect;
    use carina_core::plan::Plan;
    use carina_core::resource::Resource;

    // Let-bound resource with `directives { provider = management }`:
    // the parser stamps `provider_instance = Some("management")` on
    // its ResourceId.
    let resource = Resource::with_provider(
        "aws",
        "route53.RecordSet",
        "r.delegation_ns",
        Some("management".to_string()),
    );
    let mut plan = Plan::new();
    plan.add(Effect::Create(resolved(resource.clone())));

    // The import block address has no routing slot (`StateBlockAddress`
    // is routing-agnostic by construction). The downstream resolver
    // is responsible for lifting routing from the matched Create.
    let state_blocks = vec![StateBlock::Import {
        to: StateBlockAddress::new("aws", "route53.RecordSet", "r.delegation_ns"),
        id: Value::Concrete(ConcreteValue::String(
            "|hosted-zone-id|registry-dev.carina-rs.dev|NS"
                .to_string()
                .into(),
        )),
    }];

    let bindings = carina_core::binding_index::ResolvedBindings::default();
    let no_upstreams: std::collections::HashSet<&str> = std::collections::HashSet::new();
    add_state_block_effects(
        &mut plan,
        &state_blocks,
        &None,
        &[],
        &[resource],
        &bindings,
        &no_upstreams,
    );

    let effects = plan.effects();
    assert_eq!(
        effects.len(),
        1,
        "expected only the Import effect (Create must be suppressed when \
         the same address is being imported), got {effects:?}",
    );
    match &effects[0] {
        Effect::Import { id, .. } => {
            assert_eq!(
                id.provider_instance.as_deref(),
                Some("management"),
                "the import effect must inherit the routed instance from the \
                 matched resource so apply-time routing sends the import call \
                 to the correct provider instance",
            );
        }
        other => panic!("expected Import effect, got {other:?}"),
    }
}

#[test]
fn test_materialize_moved_states_warns_on_missing_from() {
    use carina_state::state::{ResourceState, StateFile};

    let state_blocks = vec![StateBlock::Moved {
        addresses: MovedAddresses::new("awscc", "s3.Bucket", "old_bucket", "new_bucket"),
    }];
    let mut warnings = Vec::new();
    let moved_pairs = materialize_moved_states_with_warning_sink(
        &mut HashMap::new(),
        &mut Orphans::new(),
        &mut HashMap::new(),
        &mut crate::empty_lifted_saved_attrs(),
        &state_blocks,
        &Some(StateFile::new()),
        &mut |warning| warnings.push(warning),
    )
    .expect("empty fixture has no mismatched move route");
    assert!(moved_pairs.is_empty());
    assert_eq!(
        warnings,
        vec![
            "warning: moved block from awscc.s3.Bucket 'old_bucket' to awscc.s3.Bucket 'new_bucket' was not applied: old_bucket not found in state"
                .to_string()
        ],
    );

    let mut state_file = StateFile::new();
    state_file
        .upsert_resource(ResourceState::new("s3.Bucket", "new_bucket", "awscc"))
        .expect("test state setup must be valid");
    let mut warnings = Vec::new();
    let moved_pairs = materialize_moved_states_with_warning_sink(
        &mut HashMap::new(),
        &mut Orphans::new(),
        &mut HashMap::new(),
        &mut crate::empty_lifted_saved_attrs(),
        &state_blocks,
        &Some(state_file),
        &mut |warning| warnings.push(warning),
    )
    .expect("fixture move routes must agree with current state");
    assert!(moved_pairs.is_empty());
    assert!(
        warnings.is_empty(),
        "already-applied from-absent/to-present moved block stays silent"
    );
}

fn bucket_id(name: &str) -> ResolvedResourceId {
    ResolvedResourceId::with_provider_identity("awscc", "s3.Bucket", test_identity(name), None)
}

fn bucket_resource(name: &str) -> Resource {
    Resource::with_provider("awscc", "s3.Bucket", test_identity(name), None)
}

fn resolved_desired_ids(resources: &[Resource]) -> ResolvedDesiredIds {
    ResolvedDesiredIds::from_resources(resources).expect("test desired identities must be resolved")
}

fn bucket_state_file(names: &[&str]) -> carina_state::state::StateFile {
    use carina_state::state::{ResourceState, StateFile};

    let mut state_file = StateFile::new();
    for name in names {
        state_file
            .upsert_resource(ResourceState::new(
                "s3.Bucket",
                test_identity(*name),
                "awscc",
            ))
            .expect("test state setup must be valid");
    }
    state_file
}

fn assert_collision_contains(result: Result<(), AppError>, needle: &str) {
    let err = result.expect_err("expected collision error");
    assert!(
        matches!(err, AppError::Validation(_)),
        "collision errors must be validation errors, got {err:?}"
    );
    let message = err.to_string();
    assert!(
        message.contains(needle),
        "expected error to contain {needle:?}, got {message:?}"
    );
}

#[tokio::test]
async fn test_plan_fails_on_moved_from_colliding_with_desired() {
    use carina_core::parser::ProviderContext;

    let source = r#"
let live = awscc.s3.Bucket {
    bucket_name = "live"
}

moved {
    from = awscc.s3.Bucket "live"
    to   = awscc.s3.Bucket "renamed"
}
"#;
    let parsed = parse(source, &ProviderContext::default()).expect("parse fixture");
    let state_file = Some(bucket_state_file(&["live"]));
    let StateBlockResolution {
        claims: state_block_claims,
        targets: resolved_state_block_targets,
    } = resolve_state_blocks(
        &parsed.state_blocks,
        &state_file,
        &parsed.resources,
        &carina_core::schema::SchemaRegistry::new(),
    );
    let tmp = tempfile::tempdir().expect("tempdir");
    let provider_configs = resolved_provider_configs(&parsed);

    let err = match create_plan_from_parsed_with_upstream_for_test(
        &parsed,
        &provider_configs,
        &parsed.resources,
        &state_file,
        false,
        &HashMap::new(),
        &state_block_claims,
        &resolved_state_block_targets,
        tmp.path(),
    )
    .await
    {
        Ok(_) => panic!("plan must fail before producing a green move plan"),
        Err(err) => err,
    };
    let message = err.to_string();
    assert!(
        matches!(err, AppError::Validation(_)),
        "plan collision errors must be validation errors, got {err:?}"
    );
    assert!(
        message.contains(
            "moved/rename pair from awscc.s3.Bucket live collides with a desired resource"
        ),
        "unexpected plan error: {message}"
    );
}

#[tokio::test]
async fn test_plan_fails_on_removed_from_colliding_with_expanded_loop_child() {
    use carina_core::parser::ProviderContext;
    use carina_state::state::{ResourceState, StateFile};

    let source = r#"
let cert = aws.acm.Certificate {
    domain_name       = "r.example.com"
    validation_method = "DNS"
}

let records = for (_, opt) in cert.domain_validation_options {
    aws.route53.RecordSet {
        name             = opt.resource_record.name
        type             = "CNAME"
        resource_records = [opt.resource_record.value]
    }
}

removed {
    from = aws.route53.RecordSet "records[0]"
}
"#;
    let parsed = parse(source, &ProviderContext::default()).expect("parse removed fixture");
    let state_file = {
        let mut sf = StateFile::new();
        sf.upsert_resource(
            ResourceState::try_applied("acm.Certificate", "cert", "aws", "cert-arn")
                .expect("fixture identifier is non-empty")
                .with_attribute(
                    "domain_validation_options",
                    serde_json::json!([
                        {
                            "resource_record": {
                                "name": "_a1.r.example.com",
                                "value": "_a1.acm-validations.aws."
                            }
                        }
                    ]),
                ),
        )
        .expect("test state setup must be valid");
        sf.upsert_resource(
            ResourceState::try_applied("route53.RecordSet", "records[0]", "aws", "record-set-id")
                .expect("fixture identifier is non-empty"),
        )
        .expect("test state setup must be valid");
        Some(sf)
    };
    let StateBlockResolution {
        claims: state_block_claims,
        targets: resolved_state_block_targets,
    } = resolve_state_blocks(
        &parsed.state_blocks,
        &state_file,
        &parsed.resources,
        &carina_core::schema::SchemaRegistry::new(),
    );
    let tmp = tempfile::tempdir().expect("tempdir");
    let provider_configs = resolved_provider_configs(&parsed);

    let err = match create_plan_from_parsed_with_upstream_for_test(
        &parsed,
        &provider_configs,
        &parsed.resources,
        &state_file,
        false,
        &HashMap::new(),
        &state_block_claims,
        &resolved_state_block_targets,
        tmp.path(),
    )
    .await
    {
        Ok(_) => panic!("plan must fail once deferred-for children are in the desired set"),
        Err(err) => err,
    };
    let message = err.to_string();
    assert!(
        message.contains("removed block from aws.route53.RecordSet"),
        "unexpected plan error: {message}"
    );
    assert!(
        message.contains("collides with desired resource"),
        "unexpected plan error: {message}"
    );
}

fn detected_move_identity_pairs(moves: &[ResolvedMove]) -> Vec<(String, String)> {
    moves
        .iter()
        .map(|resolved_move| {
            (
                resolved_move.from().identity_str().to_string(),
                resolved_move.to().identity_str().to_string(),
            )
        })
        .collect()
}

fn test_detected_bucket_move(from: &str, to: &str) -> ResolvedMove {
    ResolvedMove::from_moved_block(
        &MovedAddresses::new(
            "awscc",
            "s3.Bucket",
            test_identity(from.to_string()),
            test_identity(to.to_string()),
        ),
        bucket_id(from),
        bucket_id(to),
    )
    .expect("test move addresses must match their resolved ids")
}

#[test]
fn resource_move_invariants_missing_detected_source_is_an_error() {
    let mut addressed = AddressedState {
        managed_current: HashMap::new(),
        orphans: Orphans::new(),
        saved_attrs: crate::empty_lifted_saved_attrs(),
        prev_explicit: HashMap::new(),
        moved_pairs: Vec::new(),
    };

    let result = apply_detected_resource_moves(
        &mut addressed,
        vec![test_detected_bucket_move("missing", "target")],
        None,
        MoveOverlapPolicy::ComposeForward,
    );

    assert!(
        matches!(
            result,
            Err(AppError::MoveSourceNotLoaded { from, to })
                if from.identity_str() == "missing" && to.identity_str() == "target"
        ),
        "a missing move source must fail with the typed invariant error"
    );
    assert!(
        addressed.moved_pairs.is_empty(),
        "a failed transfer must not record a move"
    );
}

#[test]
fn explicit_move_accepts_deposed_only_source_that_is_present_in_state() {
    use carina_core::parser::ProviderContext;
    use carina_state::DeposedKey;
    use carina_state::state::{DeposedInstance, ResourceState, StateFile};

    let parsed = parse(
        r#"
            moved {
                from = awscc.s3.Bucket 'old_bucket'
                to   = awscc.s3.Bucket 'new_bucket'
            }
        "#,
        &ProviderContext::default(),
    )
    .expect("same-type moved block must parse");
    let from = bucket_id("old_bucket");
    let to = bucket_id("new_bucket");
    let mut deposed_only = ResourceState::new("s3.Bucket", from.identity().clone(), "awscc");
    deposed_only
        .deposed
        .push(DeposedInstance::from_persisted_json(
            DeposedKey::new_unique(),
            "bucket-deposed".to_string(),
            None,
            HashMap::new(),
            Default::default(),
        ));
    let mut state_file = StateFile::new();
    state_file
        .upsert_resource(deposed_only)
        .expect("test state setup must be valid");
    let mut managed_current = HashMap::from([(
        to.as_inner().clone(),
        Current::absent(to.as_inner().clone()),
    )]);
    let mut orphans = state_file
        .build_orphans(&HashSet::from([to.as_inner().clone()]))
        .into_plan_orphans();

    let moved_pairs = materialize_moved_states(
        &mut managed_current,
        &mut orphans,
        &mut HashMap::new(),
        &mut crate::empty_lifted_saved_attrs(),
        &parsed.state_blocks,
        &Some(state_file),
    )
    .expect("a persisted deposed-only source is a valid move source");

    assert_eq!(moved_pairs, vec![(from, to.clone())]);
    assert!(matches!(
        managed_current.get(to.as_inner()),
        Some(Current::Absent(state)) if state.state().id == *to.as_inner()
    ));
}

#[test]
fn detected_move_accepts_deposed_only_source_that_is_present_in_state() {
    use carina_state::DeposedKey;
    use carina_state::state::{DeposedInstance, ResourceState, StateFile};

    let from = bucket_id("old_bucket");
    let to = bucket_id("new_bucket");
    let mut deposed_only = ResourceState::new("s3.Bucket", from.identity().clone(), "awscc");
    deposed_only
        .deposed
        .push(DeposedInstance::from_persisted_json(
            DeposedKey::new_unique(),
            "bucket-deposed".to_string(),
            None,
            HashMap::new(),
            Default::default(),
        ));
    let mut state_file = StateFile::new();
    state_file
        .upsert_resource(deposed_only)
        .expect("test state setup must be valid");
    let mut addressed = AddressedState {
        managed_current: HashMap::new(),
        orphans: Orphans::new(),
        saved_attrs: crate::empty_lifted_saved_attrs(),
        prev_explicit: HashMap::new(),
        moved_pairs: Vec::new(),
    };

    apply_detected_resource_moves(
        &mut addressed,
        vec![test_detected_bucket_move("old_bucket", "new_bucket")],
        Some(&state_file),
        MoveOverlapPolicy::ComposeForward,
    )
    .expect("a persisted deposed-only source is a valid detected move source");

    assert_eq!(addressed.moved_pairs(), &[(from, to)]);
}

#[test]
fn resource_move_invariants_chained_moves_are_composed() {
    let source = bucket_id("source");
    let intermediate = bucket_id("intermediate");
    let target = bucket_id("target");
    let mut addressed = AddressedState {
        managed_current: HashMap::from([(
            source.as_inner().clone(),
            Current::absent(source.as_inner().clone()),
        )]),
        orphans: Orphans::new(),
        saved_attrs: crate::empty_lifted_saved_attrs(),
        prev_explicit: HashMap::new(),
        moved_pairs: Vec::new(),
    };

    apply_detected_resource_moves(
        &mut addressed,
        vec![
            test_detected_bucket_move("source", "intermediate"),
            test_detected_bucket_move("intermediate", "target"),
        ],
        None,
        MoveOverlapPolicy::ComposeForward,
    )
    .expect("both links have a loaded source");

    assert_eq!(addressed.moved_pairs, vec![(source, target.clone())]);
    assert!(
        addressed.managed_current.contains_key(target.as_inner()),
        "the state maps must be re-keyed to the final destination"
    );
    assert!(
        !addressed
            .managed_current
            .contains_key(intermediate.as_inner())
    );
}

#[test]
fn resource_move_invariants_detected_target_cannot_be_an_explicit_source() {
    let explicit_source = bucket_id("explicit_source");
    let explicit_target = bucket_id("explicit_target");
    let detected_source = bucket_id("detected_source");
    let mut addressed = AddressedState {
        managed_current: HashMap::from([(
            detected_source.as_inner().clone(),
            Current::absent(detected_source.as_inner().clone()),
        )]),
        orphans: Orphans::new(),
        saved_attrs: crate::empty_lifted_saved_attrs(),
        prev_explicit: HashMap::new(),
        moved_pairs: vec![(explicit_source.clone(), explicit_target.clone())],
    };

    let detected = DetectedAutomaticMoves::from_moves(vec![test_detected_bucket_move(
        "detected_source",
        "explicit_source",
    )]);
    let result = detected.apply_after_explicit(&mut addressed, None);

    assert!(
        matches!(
            result,
            Err(AppError::DetectedMoveConflict {
                from,
                to,
                existing_from,
                existing_to,
            }) if *from == detected_source
                && *to == explicit_source
                && *existing_from == explicit_source
                && *existing_to == explicit_target
        ),
        "the pre-refresh projection cannot be applied across an explicit move"
    );
    assert_eq!(
        addressed.moved_pairs,
        vec![(explicit_source.clone(), explicit_target)]
    );
    assert!(
        addressed
            .managed_current
            .contains_key(detected_source.as_inner()),
        "conflict detection must run before state is re-keyed"
    );
    assert!(
        !addressed
            .managed_current
            .contains_key(explicit_source.as_inner())
    );
}

#[test]
fn resource_move_invariants_detected_source_cannot_be_an_explicit_target() {
    let explicit_source = bucket_id("explicit_source");
    let explicit_target = bucket_id("explicit_target");
    let detected_target = bucket_id("detected_target");
    let mut addressed = AddressedState {
        managed_current: HashMap::from([(
            explicit_target.as_inner().clone(),
            Current::absent(explicit_target.as_inner().clone()),
        )]),
        orphans: Orphans::new(),
        saved_attrs: crate::empty_lifted_saved_attrs(),
        prev_explicit: HashMap::new(),
        moved_pairs: vec![(explicit_source.clone(), explicit_target.clone())],
    };

    let detected = DetectedAutomaticMoves::from_moves(vec![test_detected_bucket_move(
        "explicit_target",
        "detected_target",
    )]);
    let result = detected.apply_after_explicit(&mut addressed, None);

    assert!(
        matches!(
            result,
            Err(AppError::DetectedMoveConflict {
                from,
                to,
                existing_from,
                existing_to,
            }) if *from == explicit_target
                && *to == detected_target
                && *existing_from == explicit_source
                && *existing_to == explicit_target
        ),
        "an automatic projection cannot compose through an explicit move"
    );
    assert_eq!(
        addressed.moved_pairs,
        vec![(explicit_source, explicit_target.clone())]
    );
    assert!(
        addressed
            .managed_current
            .contains_key(explicit_target.as_inner()),
        "conflict detection must run before state is re-keyed"
    );
    assert!(
        !addressed
            .managed_current
            .contains_key(detected_target.as_inner())
    );
}

#[test]
fn resource_move_invariants_late_move_can_extend_an_early_chain() {
    let source = bucket_id("source");
    let intermediate = bucket_id("intermediate");
    let target = bucket_id("target");
    let mut addressed = AddressedState {
        managed_current: HashMap::from([(
            intermediate.as_inner().clone(),
            Current::absent(intermediate.as_inner().clone()),
        )]),
        orphans: Orphans::new(),
        saved_attrs: crate::empty_lifted_saved_attrs(),
        prev_explicit: HashMap::new(),
        moved_pairs: vec![(source.clone(), intermediate.clone())],
    };

    apply_detected_resource_moves(
        &mut addressed,
        vec![test_detected_bucket_move("intermediate", "target")],
        None,
        MoveOverlapPolicy::ComposeForward,
    )
    .expect("late detection may extend the already-applied early move");

    assert_eq!(addressed.moved_pairs, vec![(source, target.clone())]);
    assert!(addressed.managed_current.contains_key(target.as_inner()));
    assert!(
        !addressed
            .managed_current
            .contains_key(intermediate.as_inner())
    );
}

#[test]
fn test_plan_fails_on_two_moves_to_same_target() {
    let desired = Vec::new();
    let state_file = Some(bucket_state_file(&["old_a", "old_b"]));
    let moved_pairs = vec![
        (bucket_id("old_a"), bucket_id("target")),
        (bucket_id("old_b"), bucket_id("target")),
    ];

    assert_collision_contains(
        validate_plan_time_state_block_collisions(
            &resolved_desired_ids(&desired),
            &moved_pairs,
            &ResolvedStateBlockTargets::default(),
            &state_file,
        ),
        "two moved/rename pairs resolve to the same target awscc.s3.Bucket target",
    );
}

#[test]
fn test_plan_fails_on_two_moves_from_same_source() {
    let desired = Vec::new();
    let state_file = Some(bucket_state_file(&["old"]));
    let moved_pairs = vec![
        (bucket_id("old"), bucket_id("target_a")),
        (bucket_id("old"), bucket_id("target_b")),
    ];

    assert_collision_contains(
        validate_plan_time_state_block_collisions(
            &resolved_desired_ids(&desired),
            &moved_pairs,
            &ResolvedStateBlockTargets::default(),
            &state_file,
        ),
        "two moved/rename pairs share the same source awscc.s3.Bucket old",
    );
}

#[test]
fn test_plan_fails_on_removed_from_colliding_with_desired() {
    let ctx = WiringContext::new_for_test(vec![Box::new(AssociationCreateOnlyFactory)], &[]);
    let desired_name = "awscc_ec2_subnet_route_table_association_aaaaaaaa";
    let orphan_name = "awscc_ec2_subnet_route_table_association_bbbbbbbb";
    let mut resource = Resource::with_provider(
        "awscc",
        "ec2.SubnetRouteTableAssociation",
        desired_name,
        None,
    );
    resource.set_attr(
        "route_table_id".to_string(),
        Value::Concrete(ConcreteValue::String("rtb-private".into())),
    );
    resource.set_attr(
        "subnet_id".to_string(),
        Value::Concrete(ConcreteValue::String("subnet-current".into())),
    );
    let desired = vec![resource];
    let mut state = StateFile::new();
    state
        .upsert_resource(association_state(
            desired_name,
            "rtb-private",
            "subnet-current",
        ))
        .expect("test state setup must be valid");
    state
        .upsert_resource(association_state(
            orphan_name,
            "rtb-private",
            "subnet-orphan",
        ))
        .expect("test state setup must be valid");
    let state_file = Some(state.clone());
    let blocks = vec![StateBlock::Removed {
        from: StateBlockAddress::new("awscc", "ec2.SubnetRouteTableAssociation", desired_name),
    }];
    let resolution = resolve_state_blocks(&blocks, &state_file, &desired, ctx.schemas());
    let state_view = AnonymousReconciliationStateView::new(&ctx, &state, &[]);
    let moves = reconcile_anonymous_identifiers_with_ctx(
        &ctx,
        &desired,
        &[],
        &state_view,
        &resolution.claims,
    );

    assert!(moves.is_empty());
    assert_eq!(desired[0].id.identity_str(), Some(desired_name));

    assert_collision_contains(
        validate_plan_time_state_block_collisions(
            &resolved_desired_ids(&desired),
            &[],
            &resolution.targets,
            &state_file,
        ),
        "removed block from awscc.ec2.SubnetRouteTableAssociation awscc_ec2_subnet_route_table_association_aaaaaaaa collides with desired resource awscc.ec2.SubnetRouteTableAssociation awscc_ec2_subnet_route_table_association_aaaaaaaa",
    );
}

#[test]
fn test_plan_fails_on_move_onto_occupied_state_entry() {
    let desired = Vec::new();
    let state_file = Some(bucket_state_file(&["old", "occupied"]));
    let moved_pairs = vec![(bucket_id("old"), bucket_id("occupied"))];

    assert_collision_contains(
        validate_plan_time_state_block_collisions(
            &resolved_desired_ids(&desired),
            &moved_pairs,
            &ResolvedStateBlockTargets::default(),
            &state_file,
        ),
        "moved/rename pair awscc.s3.Bucket old -> awscc.s3.Bucket occupied would overwrite an existing state entry",
    );
}

#[test]
fn test_plan_allows_from_absent_to_present_idempotent_noop() {
    let desired = Vec::new();
    let state_file = Some(bucket_state_file(&["already_moved"]));
    let state_blocks = vec![StateBlock::Moved {
        addresses: MovedAddresses::new("awscc", "s3.Bucket", "old_name", "already_moved"),
    }];
    let mut warnings = Vec::new();

    let moved_pairs = materialize_moved_states_with_warning_sink(
        &mut HashMap::new(),
        &mut Orphans::new(),
        &mut HashMap::new(),
        &mut crate::empty_lifted_saved_attrs(),
        &state_blocks,
        &state_file,
        &mut |warning| warnings.push(warning),
    )
    .expect("fixture move routes must agree with current state");
    assert!(
        moved_pairs.is_empty(),
        "already-applied moved block must not produce a move pair"
    );
    assert!(
        warnings.is_empty(),
        "already-applied moved block must stay silent"
    );

    validate_plan_time_state_block_collisions(
        &resolved_desired_ids(&desired),
        &moved_pairs,
        &ResolvedStateBlockTargets::default(),
        &state_file,
    )
    .expect("from-absent/to-present no-op must not error");
}

#[test]
fn test_plan_fails_on_synthesized_rename_colliding_with_moved_to() {
    let desired = Vec::new();
    let state_file = Some(bucket_state_file(&["operator_source", "anonymous_source"]));
    let operator_moved_pair = (bucket_id("operator_source"), bucket_id("named"));
    let synthesized_rename_pair = (bucket_id("anonymous_source"), bucket_id("named"));
    let moved_pairs = vec![operator_moved_pair, synthesized_rename_pair];

    assert_collision_contains(
        validate_plan_time_state_block_collisions(
            &resolved_desired_ids(&desired),
            &moved_pairs,
            &ResolvedStateBlockTargets::default(),
            &state_file,
        ),
        "two moved/rename pairs resolve to the same target awscc.s3.Bucket named",
    );
}

#[test]
fn test_plan_fails_on_orphan_self_move() {
    let desired = Vec::new();
    let state_file = Some(bucket_state_file(&["orphan"]));
    let moved_pairs = vec![(bucket_id("orphan"), bucket_id("orphan"))];

    assert_collision_contains(
        validate_plan_time_state_block_collisions(
            &resolved_desired_ids(&desired),
            &moved_pairs,
            &ResolvedStateBlockTargets::default(),
            &state_file,
        ),
        "moved block from and to name the same address awscc.s3.Bucket orphan: a self-move cannot transfer state",
    );
}

#[test]
fn test_plan_fails_on_rotation_shape() {
    let desired = vec![bucket_resource("b"), bucket_resource("c")];
    let state_file = Some(bucket_state_file(&["a", "b"]));
    let moved_pairs = vec![
        (bucket_id("a"), bucket_id("b")),
        (bucket_id("b"), bucket_id("c")),
    ];

    assert_collision_contains(
        validate_plan_time_state_block_collisions(
            &resolved_desired_ids(&desired),
            &moved_pairs,
            &ResolvedStateBlockTargets::default(),
            &state_file,
        ),
        "moved/rename pair from awscc.s3.Bucket b collides with a desired resource",
    );
}

struct AssociationCreateOnlyFactory;

impl ProviderFactory for AssociationCreateOnlyFactory {
    fn name(&self) -> &str {
        "awscc"
    }

    fn display_name(&self) -> &str {
        "AWSCC association create-only test provider"
    }

    fn provider_config_attribute_types(&self) -> carina_core::schema::ValidatedConfigSchema {
        Default::default()
    }

    fn validate_config(
        &self,
        _attributes: &carina_core::provider::CanonicalProviderConfigAttributes,
    ) -> carina_core::provider::ProviderResult<()> {
        Ok(())
    }

    fn extract_region(&self, _attributes: &IndexMap<String, Value>) -> String {
        "ap-northeast-1".to_string()
    }

    fn create_provider(
        &self,
        _binding: Option<&str>,
        _config: &carina_core::provider::ProviderReadyConfig,
    ) -> carina_core::provider::BoxFuture<
        '_,
        carina_core::provider::ProviderResult<Box<dyn carina_core::provider::Provider>>,
    > {
        Box::pin(async {
            Ok(Box::new(MockProvider::new()) as Box<dyn carina_core::provider::Provider>)
        })
    }

    fn create_normalizer(
        &self,
        _binding: Option<&str>,
        _config: &carina_core::provider::ProviderReadyConfig,
    ) -> carina_core::provider::BoxFuture<
        '_,
        carina_core::provider::ProviderResult<Box<dyn carina_core::provider::ProviderNormalizer>>,
    > {
        Box::pin(async {
            Ok(Box::new(carina_core::provider::NoopNormalizer)
                as Box<dyn carina_core::provider::ProviderNormalizer>)
        })
    }

    fn schemas(&self) -> Vec<carina_core::schema::ValidatedResourceSchema> {
        let schema_drafts: Vec<carina_core::schema::ResourceSchema> = {
            use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};

            vec![
                ResourceSchema::new("s3.Bucket")
                    .with_unique_name_attribute("bucket_name")
                    .attribute(
                        AttributeSchema::optional("bucket_name", AttributeType::string())
                            .create_only()
                            .build(),
                    ),
                ResourceSchema::new("ec2.SubnetRouteTableAssociation")
                    .attribute(
                        AttributeSchema::optional("route_table_id", AttributeType::string())
                            .create_only()
                            .build(),
                    )
                    .attribute(
                        AttributeSchema::optional("subnet_id", AttributeType::string())
                            .create_only()
                            .build(),
                    ),
            ]
        };
        schema_drafts
            .into_iter()
            .map(|schema| {
                schema
                    .validate()
                    .expect("factory test schema must be valid")
            })
            .collect()
    }
}

fn route_table_state(binding: &str, id: &str) -> carina_state::state::ResourceState {
    let mut state =
        carina_state::state::ResourceState::new("ec2.RouteTable", test_identity(binding), "awscc");
    state.binding = Some(binding.to_string());
    state.set_persisted_attribute("id".to_string(), serde_json::Value::String(id.to_string()));
    state
}

fn association_state(
    name: &str,
    route_table_id: &str,
    subnet_id: &str,
) -> carina_state::state::ResourceState {
    let mut state = carina_state::state::ResourceState::new(
        "ec2.SubnetRouteTableAssociation",
        test_identity(name),
        "awscc",
    );
    state.set_persisted_attribute(
        "route_table_id".to_string(),
        serde_json::Value::String(route_table_id.to_string()),
    );
    state.set_persisted_attribute(
        "subnet_id".to_string(),
        serde_json::Value::String(subnet_id.to_string()),
    );
    state
}

fn desired_association(name: &str, route_table_binding: &str, subnet_id: &str) -> Resource {
    use carina_core::resource::AccessPath;

    let mut resource = Resource::with_provider(
        "awscc",
        "ec2.SubnetRouteTableAssociation",
        test_identity(name),
        None,
    );
    resource.set_attr(
        "route_table_id".to_string(),
        Value::Deferred(DeferredValue::ResourceRef {
            path: AccessPath::new(route_table_binding, "id"),
        }),
    );
    resource.set_attr(
        "subnet_id".to_string(),
        Value::Concrete(ConcreteValue::String(subnet_id.to_string().into())),
    );
    resource
}

#[test]
fn resource_move_invariants_anonymous_match_uses_state_row_routing() {
    use carina_state::state::StateFile;

    let ctx = WiringContext::new_for_test(vec![Box::new(AssociationCreateOnlyFactory)], &[]);
    let old_name = "ec2_subnet_route_table_association_11111111";
    let new_name = "ec2_subnet_route_table_association_aaaaaaaa";
    let mut state_file = StateFile::new();
    state_file
        .upsert_resource(route_table_state("private_rtb", "rtb-private"))
        .expect("test state setup must be valid");
    let mut prior = association_state(old_name, "rtb-private", "subnet-a");
    prior.directives.provider_instance = Some("persisted-route".to_string());
    state_file
        .upsert_resource(prior)
        .expect("test state setup must be valid");

    let mut desired = desired_association(new_name, "private_rtb", "subnet-a");
    desired.id.provider_instance = Some("desired-route".to_string());
    let state_view = AnonymousReconciliationStateView::new(&ctx, &state_file, &[]);
    let moves = reconcile_anonymous_identifiers_with_ctx(
        &ctx,
        &[desired],
        &[],
        &state_view,
        &StateBlockClaims::empty(),
    );

    assert_eq!(moves.len(), 1);
    assert_eq!(
        moves[0].from().provider_instance.as_deref(),
        Some("persisted-route"),
        "move.from must be the matched state row's routed address"
    );
    assert_eq!(
        moves[0].to().provider_instance.as_deref(),
        Some("desired-route")
    );
}

#[test]
fn resource_move_invariants_named_match_uses_state_row_routing() {
    use carina_state::state::{ResourceState, StateFile};

    let ctx = WiringContext::new_for_test(vec![Box::new(AssociationCreateOnlyFactory)], &[]);
    let old_name = "awscc_s3_bucket_11111111";
    let mut prior = ResourceState::new("s3.Bucket", test_identity(old_name), "awscc");
    prior.directives.provider_instance = Some("persisted-route".to_string());
    prior.set_persisted_attribute("bucket_name", serde_json::json!("logs"));
    let mut state_file = StateFile::new();
    state_file
        .upsert_resource(prior)
        .expect("test state setup must be valid");
    let desired = Resource::with_provider(
        "awscc",
        "s3.Bucket",
        test_identity("logs"),
        Some("desired-route".to_string()),
    )
    .with_binding("logs")
    .with_attribute(
        "bucket_name",
        Value::Concrete(ConcreteValue::String("logs".into())),
    );

    let state_view = AnonymousReconciliationStateView::new(&ctx, &state_file, &[]);
    let moves = detect_anonymous_to_named_moves_with_ctx(
        &ctx,
        &[desired],
        &[],
        &state_view,
        &StateBlockClaims::empty(),
    )
    .expect("empty provider configuration is valid for this create-only match");

    assert_eq!(moves.len(), 1);
    assert_eq!(
        moves[0].from().provider_instance.as_deref(),
        Some("persisted-route"),
        "anonymous-to-named move.from must retain the state row's route"
    );
    assert_eq!(
        moves[0].to().provider_instance.as_deref(),
        Some("desired-route")
    );
}

#[test]
fn resource_move_invariants_projected_attribute_less_row_is_not_a_candidate() {
    use carina_state::state::{ResourceState, StateFile};

    let ctx = WiringContext::new_for_test(vec![Box::new(AssociationCreateOnlyFactory)], &[]);
    let source = bucket_id("awscc_s3_bucket_11111111");
    let target = bucket_id("awscc_s3_bucket_aaaaaaaa");
    let mut state_file = StateFile::new();
    state_file
        .upsert_resource(ResourceState::new(
            "s3.Bucket",
            source.identity().clone(),
            "awscc",
        ))
        .expect("test state setup must be valid");
    state_file
        .upsert_resource(ResourceState::new(
            "s3.Bucket",
            test_identity("awscc_s3_bucket_22222222"),
            "awscc",
        ))
        .expect("test state setup must be valid");

    let state_view =
        AnonymousReconciliationStateView::new(&ctx, &state_file, &[(source, target.clone())]);
    assert!(
        state_view
            .entries_by_type(&StateBlockClaims::empty(), "awscc", "s3.Bucket")
            .candidates()
            .next()
            .is_none(),
        "a projected row without persisted attributes is existence-only"
    );

    let desired = bucket_resource(target.identity_str());
    let moves = reconcile_anonymous_identifiers_with_ctx(
        &ctx,
        &[desired],
        &[],
        &state_view,
        &StateBlockClaims::empty(),
    );
    assert!(
        moves.is_empty(),
        "the projected target must still satisfy the exact-address existence check"
    );
}

#[test]
fn resource_move_invariants_unprojected_in_flight_target_blocks_orphan_match() {
    use carina_core::provider::Sent;
    use carina_state::state::{ResourceState, StateFile};

    let ctx = WiringContext::new_for_test(vec![Box::new(AssociationCreateOnlyFactory)], &[]);
    let old_name = "ec2_subnet_route_table_association_11111111";
    let target_name = "ec2_subnet_route_table_association_aaaaaaaa";
    let desired = desired_association(target_name, "private_rtb", "subnet-a");
    let persisted_target = Resource::with_provider(
        "awscc",
        "ec2.SubnetRouteTableAssociation",
        test_identity(target_name),
        None,
    )
    .with_attribute(
        "route_table_id",
        Value::Concrete(ConcreteValue::String("rtb-private".into())),
    )
    .with_attribute(
        "subnet_id",
        Value::Concrete(ConcreteValue::String("subnet-a".into())),
    );
    let resolved_persisted_target = ResolvedResource::new(persisted_target);
    let pending = crate::fixture_in_flight_create(
        &desired.id,
        "opaque-token",
        None,
        Sent::AllRequestAttributes,
    );
    let in_flight =
        ResourceState::from_in_flight(&resolved_persisted_target, &pending, None, None, None)
            .expect("in-flight fixture must encode");
    let mut state_file = StateFile::new();
    state_file
        .upsert_resource(route_table_state("private_rtb", "rtb-private"))
        .expect("test state setup must be valid");
    state_file
        .upsert_resource(association_state(old_name, "rtb-private", "subnet-a"))
        .expect("test state setup must be valid");
    state_file
        .upsert_resource(in_flight)
        .expect("test state setup must be valid");

    let state_view = AnonymousReconciliationStateView::new(&ctx, &state_file, &[]);
    let moves = reconcile_anonymous_identifiers_with_ctx(
        &ctx,
        &[desired],
        &[],
        &state_view,
        &StateBlockClaims::empty(),
    );

    assert!(
        moves.is_empty(),
        "the existing in-flight target address must prevent rebinding an orphan onto it"
    );
}

#[test]
fn resource_move_invariants_projected_state_read_preserves_dependency_bindings() {
    use carina_state::state::StateFile;

    let ctx = WiringContext::new_for_test(vec![Box::new(AssociationCreateOnlyFactory)], &[]);
    let old_name = "ec2_subnet_route_table_association_11111111";
    let new_name = "ec2_subnet_route_table_association_aaaaaaaa";
    let mut prior = association_state(old_name, "rtb-private", "subnet-a");
    prior.dependency_bindings.insert("private_rtb".to_string());
    let mut state_file = StateFile::new();
    state_file
        .upsert_resource(route_table_state("private_rtb", "rtb-private"))
        .expect("test state setup must be valid");
    state_file
        .upsert_resource(prior)
        .expect("test state setup must be valid");
    let desired = desired_association(new_name, "private_rtb", "subnet-a");
    let resources = vec![desired];
    let state = Some(state_file);
    let mut parsed = carina_core::parser::InferredFile::default();
    parsed.resources = resources;
    let detected = DeferredForExpansionPrelude::new_for_test(&parsed, &state, ctx.schemas())
        .expect("build expansion prelude")
        .detect_moves(&ctx, &[], &[], &StateBlockClaims::empty())
        .expect("the automatic move projection must resolve");
    let (loaded_id, loaded) = detected.build_current(&parsed.resources[0].id);
    let saved = detected.saved_dependency_bindings();

    assert_eq!(loaded_id.identity_str(), Some(old_name));
    assert_eq!(loaded.id().identity_str(), Some(old_name));
    assert_eq!(
        saved.get(&parsed.resources[0].id),
        Some(&["private_rtb".to_string()].into_iter().collect()),
        "phase-1 metadata lookup must read the moved source row"
    );
}

fn assert_claimed_association_stays_orphaned_after_reconcile() {
    use carina_state::state::StateFile;

    let ctx = WiringContext::new_for_test(vec![Box::new(AssociationCreateOnlyFactory)], &[]);
    let old_name = "ec2_subnet_route_table_association_11111111";
    let desired_name = "ec2_subnet_route_table_association_aaaaaaaa";
    let mut state_file = StateFile::new();
    state_file
        .upsert_resource(route_table_state("private_rtb", "rtb-private"))
        .expect("test state setup must be valid");
    state_file
        .upsert_resource(association_state(old_name, "rtb-private", "subnet-a"))
        .expect("test state setup must be valid");
    let resources = vec![desired_association(desired_name, "private_rtb", "subnet-a")];
    let state_blocks = vec![StateBlock::Removed {
        from: StateBlockAddress::new("awscc", "ec2.SubnetRouteTableAssociation", old_name),
    }];
    let claims = resolve_state_block_claims(
        &state_blocks,
        &Some(state_file.clone()),
        &resources,
        ctx.schemas(),
    );
    assert!(
        claims
            .screen_entries(
                "awscc",
                "ec2.SubnetRouteTableAssociation",
                [old_name],
                |name| name,
            )
            .candidates()
            .next()
            .is_none(),
        "effective removed block must claim its state entry"
    );

    let state_view = AnonymousReconciliationStateView::new(&ctx, &state_file, &[]);
    let moves =
        reconcile_anonymous_identifiers_with_ctx(&ctx, &resources, &[], &state_view, &claims);

    assert!(moves.is_empty());
    assert_eq!(
        resources[0].id.identity_str().expect("resolved identity"),
        desired_name,
        "claimed state entry must not be rebound to the desired resource"
    );
    assert!(
        state_file
            .find_resource("awscc", "ec2.SubnetRouteTableAssociation", old_name)
            .is_some(),
        "claimed state entry must survive under its old name for the orphan path"
    );
}

#[test]
fn claimed_state_entry_is_excluded_from_automatic_move_detection() {
    assert_claimed_association_stays_orphaned_after_reconcile();
}

#[test]
fn destroy_and_state_refresh_identity_preparation_preserve_anonymous_addresses() {
    use carina_core::parser::InferredFile;
    use carina_core::schema::ValidatedResourceSchema;
    use carina_state::state::StateFile;

    let old_name = "ec2_subnet_route_table_association_11111111";
    let fresh_name = "ec2_subnet_route_table_association_aaaaaaaa";
    let desired = || {
        Resource::with_provider(
            "awscc",
            "ec2.SubnetRouteTableAssociation",
            test_identity(fresh_name),
            None,
        )
        .with_attribute(
            "route_table_id",
            Value::Concrete(ConcreteValue::String("rtb-private".into())),
        )
        .with_attribute(
            "subnet_id",
            Value::Concrete(ConcreteValue::String("subnet-a".into())),
        )
    };
    let state = || {
        let mut state = StateFile::new();
        state
            .upsert_resource(association_state(old_name, "rtb-private", "subnet-a"))
            .expect("test state setup must be valid");
        Some(state)
    };
    let config = || {
        let mut parsed = InferredFile::default();
        parsed.resources = vec![desired()];
        ValidatedConfig::from_factories_with_additional_schemas_for_test(
            parsed,
            vec![Box::new(AssociationCreateOnlyFactory)],
            &[],
            Vec::<(String, ValidatedResourceSchema)>::new(),
        )
    };

    let mut destroy_config = config();
    let destroy_state = state();
    destroy_config.reconcile_for_destroy(&destroy_state);
    assert!(
        destroy_state
            .as_ref()
            .and_then(|state| {
                state.find_resource("awscc", "ec2.SubnetRouteTableAssociation", old_name)
            })
            .is_some(),
        "destroy must leave the persisted row at its old address for orphan deletion"
    );
    assert_eq!(
        destroy_config.evaluated_resources_for_test().resources[0]
            .id
            .identity_str(),
        Some(fresh_name)
    );

    let mut refresh_config = config();
    let refresh_state = state();
    refresh_config.reconcile_for_state_refresh(&refresh_state);
    assert!(
        refresh_state
            .as_ref()
            .and_then(|state| {
                state.find_resource("awscc", "ec2.SubnetRouteTableAssociation", old_name)
            })
            .is_some(),
        "state refresh must keep the persisted row at its old address"
    );
    assert_eq!(
        refresh_config.evaluated_resources_for_test().resources[0]
            .id
            .identity_str(),
        Some(fresh_name)
    );
}

#[test]
fn reconcile_anonymous_identifiers_with_ctx_resolves_deferred_create_only_from_state_bindings() {
    use carina_state::state::StateFile;

    let ctx = WiringContext::new_for_test(vec![Box::new(AssociationCreateOnlyFactory)], &[]);
    let mut state_file = StateFile::new();
    state_file
        .upsert_resource(route_table_state("private_rtb", "rtb-private"))
        .expect("test state setup must be valid");
    state_file
        .upsert_resource(route_table_state("public_rtb", "rtb-public"))
        .expect("test state setup must be valid");
    state_file
        .upsert_resource(association_state(
            "ec2_subnet_route_table_association_11111111",
            "rtb-private",
            "subnet-a",
        ))
        .expect("test state setup must be valid");
    state_file
        .upsert_resource(association_state(
            "ec2_subnet_route_table_association_22222222",
            "rtb-public",
            "subnet-a",
        ))
        .expect("test state setup must be valid");
    state_file
        .upsert_resource(association_state(
            "ec2_subnet_route_table_association_33333333",
            "rtb-private",
            "subnet-c",
        ))
        .expect("test state setup must be valid");

    let resources = vec![
        desired_association(
            "ec2_subnet_route_table_association_aaaaaaaa",
            "private_rtb",
            "subnet-a",
        ),
        desired_association(
            "ec2_subnet_route_table_association_bbbbbbbb",
            "public_rtb",
            "subnet-a",
        ),
        desired_association(
            "ec2_subnet_route_table_association_cccccccc",
            "private_rtb",
            "subnet-c",
        ),
    ];

    let state_view = AnonymousReconciliationStateView::new(&ctx, &state_file, &[]);
    let moves = reconcile_anonymous_identifiers_with_ctx(
        &ctx,
        &resources,
        &[],
        &state_view,
        &carina_core::identifier::StateBlockClaims::empty(),
    );

    let names: Vec<_> = resources
        .iter()
        .map(|resource| resource.id.identity_str().expect("resolved identity"))
        .collect();
    assert_eq!(
        names,
        vec![
            "ec2_subnet_route_table_association_aaaaaaaa",
            "ec2_subnet_route_table_association_bbbbbbbb",
            "ec2_subnet_route_table_association_cccccccc",
        ],
        "desired anonymous associations must retain their freshly computed identities",
    );
    assert_eq!(
        detected_move_identity_pairs(&moves),
        vec![
            (
                "ec2_subnet_route_table_association_11111111".to_string(),
                "ec2_subnet_route_table_association_aaaaaaaa".to_string(),
            ),
            (
                "ec2_subnet_route_table_association_22222222".to_string(),
                "ec2_subnet_route_table_association_bbbbbbbb".to_string(),
            ),
            (
                "ec2_subnet_route_table_association_33333333".to_string(),
                "ec2_subnet_route_table_association_cccccccc".to_string(),
            ),
        ],
        "resolved create-only values must produce state-row moves",
    );
}

#[test]
fn reconcile_anonymous_identifiers_reserves_same_type_data_source_binding() {
    use carina_core::resource::DataSource;
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};
    use carina_state::state::{ResourceState, StateFile};

    let managed_schema = ResourceSchema::new("s3.Bucket")
        .attribute(
            AttributeSchema::optional("bucket", AttributeType::string())
                .create_only()
                .build(),
        )
        .validate()
        .expect("managed fixture schema must be valid");
    let data_source_schema = ResourceSchema::new("s3.Bucket")
        .attribute(AttributeSchema::optional("bucket", AttributeType::string()).build())
        .as_data_source()
        .validate()
        .expect("data-source fixture schema must be valid");
    let ctx = WiringContext::new_for_test_with_additional_schemas(
        Vec::new(),
        &[],
        [
            ("aws".to_string(), managed_schema),
            ("aws".to_string(), data_source_schema),
        ],
    );

    let mut state_file = StateFile::new();
    let mut prior = ResourceState::new("s3.Bucket", test_identity("logs"), "aws");
    prior.set_persisted_attribute("bucket", serde_json::json!("x"));
    state_file
        .upsert_resource(prior)
        .expect("test state setup must be valid");

    let managed = vec![
        Resource::with_provider(
            "aws",
            "s3.Bucket",
            test_identity("aws_s3_bucket_aaaaaaaa"),
            None,
        )
        .with_attribute("bucket", Value::Concrete(ConcreteValue::String("x".into()))),
    ];
    let data_sources = vec![
        DataSource::with_provider("aws", "s3.Bucket", test_identity("logs"), None)
            .with_binding("logs")
            .with_attribute("bucket", Value::Concrete(ConcreteValue::String("x".into()))),
    ];

    let state_view = AnonymousReconciliationStateView::new(&ctx, &state_file, &[]);
    let moves = reconcile_anonymous_identifiers_with_ctx(
        &ctx,
        &managed,
        &data_sources,
        &state_view,
        &StateBlockClaims::empty(),
    );

    assert!(moves.is_empty());
    assert_ne!(
        managed[0].id, data_sources[0].id,
        "anonymous managed resource must not adopt a data-source binding's ResourceId",
    );
}

#[test]
fn test_stale_moved_block_releases_claims() {
    use carina_state::state::StateFile;

    let ctx = WiringContext::new_for_test(vec![Box::new(AssociationCreateOnlyFactory)], &[]);
    let mut state_file = StateFile::new();
    state_file
        .upsert_resource(route_table_state("private_rtb", "rtb-private"))
        .expect("test state setup must be valid");
    state_file
        .upsert_resource(association_state(
            "ec2_subnet_route_table_association_11111111",
            "rtb-private",
            "subnet-a",
        ))
        .expect("test state setup must be valid");

    let resources = vec![desired_association(
        "ec2_subnet_route_table_association_aaaaaaaa",
        "private_rtb",
        "subnet-a",
    )];
    let state_blocks = vec![StateBlock::Moved {
        addresses: MovedAddresses::new(
            "awscc",
            "ec2.SubnetRouteTableAssociation",
            "does_not_exist",
            "ec2_subnet_route_table_association_aaaaaaaa",
        ),
    }];
    let claims = resolve_state_block_claims(
        &state_blocks,
        &Some(state_file.clone()),
        &resources,
        ctx.schemas(),
    );

    assert!(
        !claims.claims_to(&resources[0].id),
        "ineffective moved block must not pin its to address"
    );
    let state_view = AnonymousReconciliationStateView::new(&ctx, &state_file, &[]);
    let moves =
        reconcile_anonymous_identifiers_with_ctx(&ctx, &resources, &[], &state_view, &claims);

    assert_eq!(
        resources[0].id.identity_str().expect("resolved identity"),
        "ec2_subnet_route_table_association_aaaaaaaa",
        "reconciliation must not rewrite the desired identity"
    );
    assert_eq!(
        detected_move_identity_pairs(&moves),
        vec![(
            "ec2_subnet_route_table_association_11111111".to_string(),
            "ec2_subnet_route_table_association_aaaaaaaa".to_string(),
        )],
        "stale moved block must release claims so meaning-based matching can move the row"
    );
}

#[test]
fn moved_blocks_are_honored_before_heuristic_reconciliation_for_five_renames() {
    use carina_core::resource::ResourceId;
    use carina_state::state::StateFile;

    let ctx = WiringContext::new_for_test(vec![Box::new(AssociationCreateOnlyFactory)], &[]);
    let mut state_file = StateFile::new();
    let mut resources = Vec::new();
    let mut state_blocks = Vec::new();
    let mut current_states = HashMap::new();
    let mut old_names = Vec::new();
    let mut desired_names = Vec::new();

    for idx in 0..5 {
        let binding = format!("rtb_{idx}");
        let route_table_id = format!("rtb-{idx}");
        let subnet_id = format!("subnet-{idx}");
        let old_name = format!("ec2_subnet_route_table_association_old_{idx:08x}");
        let desired_name = format!("ec2_subnet_route_table_association_new_{idx:08x}");

        state_file
            .upsert_resource(route_table_state(&binding, &route_table_id))
            .expect("test state setup must be valid");
        state_file
            .upsert_resource(association_state(&old_name, &route_table_id, &subnet_id))
            .expect("test state setup must be valid");
        resources.push(desired_association(&desired_name, &binding, &subnet_id));
        state_blocks.push(StateBlock::Moved {
            addresses: MovedAddresses::new(
                "awscc",
                "ec2.SubnetRouteTableAssociation",
                test_identity(old_name.clone()),
                test_identity(desired_name.clone()),
            ),
        });

        let old_id = ResourceId::with_provider_identity(
            "awscc",
            "ec2.SubnetRouteTableAssociation",
            test_identity(old_name.clone()),
            None,
        );
        current_states.insert(old_id.clone(), Current::absent(old_id));
        old_names.push(old_name);
        desired_names.push(desired_name);
    }

    let claims = resolve_state_block_claims(
        &state_blocks,
        &Some(state_file.clone()),
        &resources,
        ctx.schemas(),
    );
    let state_view = AnonymousReconciliationStateView::new(&ctx, &state_file, &[]);
    let moves =
        reconcile_anonymous_identifiers_with_ctx(&ctx, &resources, &[], &state_view, &claims);
    assert!(moves.is_empty());
    assert_eq!(
        resources
            .iter()
            .map(|resource| resource
                .id
                .identity_str()
                .expect("resolved identity")
                .to_string())
            .collect::<Vec<_>>(),
        desired_names,
        "heuristics must not re-key desired resources whose names are moved.to claims"
    );

    let moved_pairs = materialize_moved_states(
        &mut current_states,
        &mut Orphans::new(),
        &mut HashMap::new(),
        &mut crate::empty_lifted_saved_attrs(),
        &state_blocks,
        &Some(state_file),
    )
    .expect("fixture move routes must agree with current state");
    assert_eq!(moved_pairs.len(), 5);
    for (idx, (from, to)) in moved_pairs.iter().enumerate() {
        assert_eq!(from.identity_str(), old_names[idx]);
        assert_eq!(to.identity_str(), desired_names[idx]);
    }
    for name in old_names {
        let id = ResourceId::with_provider_identity(
            "awscc",
            "ec2.SubnetRouteTableAssociation",
            test_identity(name),
            None,
        );
        assert!(
            !current_states.contains_key(&id),
            "old state key must be removed after materialized move"
        );
    }
    for name in desired_names {
        let id = ResourceId::with_provider_identity(
            "awscc",
            "ec2.SubnetRouteTableAssociation",
            test_identity(name),
            None,
        );
        assert!(
            current_states.contains_key(&id),
            "desired state key must exist after materialized move"
        );
    }
}

#[tokio::test]
async fn anonymous_cascade_child_create_uses_unresolved_source_after_state_identity_reconcile() {
    use carina_core::binding_index::ResolvedBindings;
    use carina_core::effect::Effect;
    use carina_core::parser::ProviderContext;
    use carina_state::state::{ResourceState, StateFile};

    fn string(value: &str) -> Value {
        Value::Concrete(ConcreteValue::String(value.to_string().into()))
    }

    fn compute_initial_anonymous_ids(
        ctx: &WiringContext,
        parsed: &mut carina_core::parser::ParsedFile,
    ) {
        let canonical =
            canonicalize_resources_with_schemas(&mut parsed.resources, ctx.schemas()).unwrap();
        let errors = compute_anonymous_identifiers_with_ctx(ctx, canonical, &parsed.providers);
        assert!(errors.is_empty(), "anonymous id setup failed: {errors:?}");
    }

    fn concrete_subnet_identity(ctx: &WiringContext, providers: &[ProviderConfig]) -> String {
        let mut resources = vec![
            Resource::pending_with_provider("awscc", "ec2.Subnet", None)
                .with_attribute("vpc_id", string("vpc-old"))
                .with_attribute("cidr_block", string("10.220.1.0/24"))
                .with_attribute("availability_zone", string("ap-northeast-1c")),
        ];
        let canonical = canonicalize_resources_with_schemas(&mut resources, ctx.schemas()).unwrap();
        let errors = compute_anonymous_identifiers_with_ctx(ctx, canonical, providers);
        assert!(errors.is_empty(), "state id setup failed: {errors:?}");
        resources[0]
            .id
            .identity_str()
            .expect("resolved identity")
            .to_string()
    }

    let source = r#"
        provider awscc {
        }

        let vpc = awscc.ec2.Vpc {
            cidr_block = "10.221.0.0/16"
            directives {
                create_before_destroy = true
            }
        }

        awscc.ec2.Subnet {
            vpc_id = vpc.vpc_id
            cidr_block = "10.221.1.0/24"
            availability_zone = "ap-northeast-1c"
        }
    "#;

    let wiring = ValidatedWiring::from_factories_for_test(vec![Box::new(CascadeAwsccFactory)], &[]);
    let ctx = wiring.context();
    let mut parsed = parse(source, &ProviderContext::default()).expect("parse fixture");
    let mut unresolved_parsed = parsed.clone();
    compute_initial_anonymous_ids(ctx, &mut parsed);
    compute_initial_anonymous_ids(ctx, &mut unresolved_parsed);

    let unresolved_subnet_key_before_plan = unresolved_parsed
        .resources
        .iter()
        .find(|resource| resource.id.resource_type == "ec2.Subnet")
        .expect("anonymous subnet should parse")
        .id
        .clone();
    let state_subnet_identity = concrete_subnet_identity(ctx, &parsed.providers);

    let mut vpc_state = ResourceState::try_applied("ec2.Vpc", "vpc", "awscc", "vpc-old")
        .expect("fixture identifier is non-empty")
        .with_attribute("cidr_block", serde_json::json!("10.220.0.0/16"))
        .with_attribute("vpc_id", serde_json::json!("vpc-old"));
    vpc_state.binding = Some("vpc".to_string());

    let mut subnet_state = ResourceState::try_applied(
        "ec2.Subnet",
        test_identity(&state_subnet_identity),
        "awscc",
        "subnet-old",
    )
    .expect("fixture identifier is non-empty")
    .with_attribute("vpc_id", serde_json::json!("vpc-old"))
    .with_attribute("cidr_block", serde_json::json!("10.220.1.0/24"))
    .with_attribute("availability_zone", serde_json::json!("ap-northeast-1c"));
    subnet_state.dependency_bindings.insert("vpc".to_string());

    let mut state_file = StateFile::new();
    state_file
        .upsert_resource(vpc_state)
        .expect("test state setup must be valid");
    state_file
        .upsert_resource(subnet_state)
        .expect("test state setup must be valid");

    let provider_configs = resolved_provider_configs(&parsed);
    let canonical_resources = canonical_pair(
        &parsed,
        unresolved_parsed.resources.clone(), // allow: direct — plan-time reconciliation
        ctx.schemas(),
    );
    let validated = ValidatedConfig::from_wiring_ref_for_test(&wiring, canonical_resources);
    let plan_ctx = create_plan_from_parsed_with_upstream_with_ctx(
        validated,
        &provider_configs,
        &Some(state_file),
        false,
        ProgressSink::Human,
        &HashMap::new(),
        &StateBlockClaims::empty(),
        &ResolvedStateBlockTargets::default(),
    )
    .await
    .expect("plan should build");
    let plan_ctx = plan_ctx.into_context_for_test();

    let subnet_create = plan_ctx
        .plan
        .effects()
        .iter()
        .find_map(|effect| match effect {
            Effect::Create(resource) if resource.id.resource_type == "ec2.Subnet" => {
                Some(resource.id.clone())
            }
            _ => None,
        })
        .unwrap_or_else(|| {
            panic!(
                "plan should contain anonymous subnet replacement create; summary: {:?}; effects: {:?}; errors: {:?}",
                plan_ctx.plan.summary(),
                plan_ctx.plan.effects(),
                plan_ctx.plan.errors()
            )
        });
    let unresolved_keys: Vec<String> = plan_ctx
        .unresolved_resources
        .iter()
        .map(|resource| resource.as_resource().id.to_string())
        .collect();

    let unresolved_resources: HashMap<ResourceId, UnresolvedResource> = plan_ctx
        .unresolved_resources
        .iter()
        .map(|resource| (resource.as_resource().id.clone(), resource.clone()))
        .collect();
    let sorted_resources: Vec<Resource> = plan_ctx
        .sorted_resources
        .iter()
        .map(|resource| resource.as_inner().clone())
        .collect();
    let plan_input_states = carina_core::resource::into_plan_input_current_map(
        plan_ctx.managed_current.clone(),
        ctx.schemas(),
        &sorted_resources,
    );
    let bindings = ResolvedBindings::pre_apply_live(LivePreApplyInputs {
        managed: &sorted_resources,
        compositions: &[],
        managed_current: &plan_input_states,
        data_source_values: &plan_ctx.data_source_values,
        remote_bindings: &HashMap::new(),
        wait_aliases: &[],
    });

    let executable_data_source_values = plan_ctx
        .data_source_values
        .clone()
        .into_executable()
        .expect("fixture plan must not contain blocked data sources");
    let provider = CascadeCreateProvider::default();
    let input = LiveExecutionInput {
        plan: &plan_ctx.plan,
        provider_check_inputs: ProviderCheckInputs::PlanNormalized {
            resource_origins: &unresolved_resources,
            data_source_origins: &plan_ctx.data_sources,
        },
        compositions: &[],
        bindings,
        managed_current: plan_ctx.managed_current,
        data_source_values: executable_data_source_values,
        normalizer: &carina_core::provider::NoopNormalizer,
        provider_configs: &parsed.providers,
        factories: ctx.factories(),
        schemas: ctx.schemas(),
        parallelism: carina_core::executor::TEST_UNCAPPED,
        precompleted_success_count: 0,
        checkpoint_sink: carina_core::executor::ExecutionCheckpointSink::noop(),
    };
    let outcome = execute_live_plan(
        &provider,
        input,
        &NoopExecutionObserver,
        carina_core::shutdown::ShutdownToken::running(),
    )
    .await;
    let result = match outcome {
        ExecutionOutcome::Completed(result) => result,
        ExecutionOutcome::Cancelled(_) => panic!("test plan should not be cancelled"),
    };
    assert_eq!(result.failure_count, 0);

    let create_calls = provider.create_calls();
    let subnet_call = create_calls
        .iter()
        .find(|(id, _)| id.contains(".ec2.Subnet."))
        .unwrap_or_else(|| panic!("subnet create missing from calls: {create_calls:?}"));
    assert_eq!(
        subnet_call.1.get("vpc_id"),
        Some(&string("vpc-new")),
        "anonymous subnet create must re-resolve vpc.vpc_id from the replacement VPC state. \
         unresolved key before plan: {unresolved_subnet_key_before_plan}; \
         state identity: {state_subnet_identity}; effect id: {subnet_create}; \
         unresolved map keys: {unresolved_keys:?}; calls: {create_calls:?}"
    );
}

#[tokio::test]
async fn moved_blocks_plan_level_fixture_emits_five_moves_only() {
    use carina_core::effect::Effect;
    use carina_core::parser::ProviderContext;
    use carina_state::state::{ResourceState, StateFile};

    // This plan-level fixture pins moved materialization and Move-effect
    // emission at the `create_plan_from_parsed_with_upstream_with_ctx` seam. The
    // claims-precedence ordering itself is covered by
    // `moved_blocks_are_honored_before_heuristic_reconciliation_for_five_renames`.
    let mut source = String::new();
    let mut state_file = StateFile::new();
    for idx in 0..5 {
        let old_name = format!("old_assoc_{idx}");
        let desired_name = format!("desired_assoc_{idx}");
        let route_table_id = format!("rtb-{idx}");
        let subnet_id = format!("subnet-{idx}");

        source.push_str(&format!(
            r#"
let {desired_name} = awscc.ec2.SubnetRouteTableAssociation {{
    route_table_id = "{route_table_id}"
    subnet_id      = "{subnet_id}"
}}

moved {{
    from = awscc.ec2.SubnetRouteTableAssociation "{old_name}"
    to   = awscc.ec2.SubnetRouteTableAssociation "{desired_name}"
}}
"#
        ));

        state_file
            .upsert_resource(
                ResourceState::try_applied(
                    "ec2.SubnetRouteTableAssociation",
                    test_identity(old_name),
                    "awscc",
                    format!("assoc-{idx}"),
                )
                .expect("fixture identifier is non-empty")
                .with_attribute("route_table_id", serde_json::Value::String(route_table_id))
                .with_attribute("subnet_id", serde_json::Value::String(subnet_id)),
            )
            .expect("test state setup must be valid");
    }

    let parsed = parse(&source, &ProviderContext::default()).expect("parse fixture");
    let StateBlockResolution {
        claims: state_block_claims,
        targets: resolved_state_block_targets,
    } = resolve_state_blocks(
        &parsed.state_blocks,
        &Some(state_file.clone()),
        &parsed.resources,
        &carina_core::schema::SchemaRegistry::new(),
    );
    let tmp = tempfile::tempdir().expect("tempdir");
    let provider_configs = resolved_provider_configs(&parsed);
    let ctx = create_plan_from_parsed_with_upstream_for_test(
        &parsed,
        &provider_configs,
        &parsed.resources,
        &Some(state_file),
        false,
        &HashMap::new(),
        &state_block_claims,
        &resolved_state_block_targets,
        tmp.path(),
    )
    .await
    .expect("plan fixture");

    let summary = ctx.plan.summary();
    assert_eq!(summary.create, 0, "moved fixture must not add resources");
    assert_eq!(
        summary.delete, 0,
        "moved fixture must not destroy resources"
    );
    assert_eq!(
        summary.replace, 0,
        "moved fixture must not replace resources"
    );
    assert_eq!(summary.update, 0, "moved fixture must not update resources");
    assert_eq!(summary.moved, 5, "all five moved blocks must be honored");
    assert_eq!(ctx.plan.mutation_count(), 5);
    assert!(
        ctx.plan
            .effects()
            .iter()
            .all(|effect| matches!(effect, Effect::Move { .. })),
        "plan must contain only Move effects, got {:?}",
        ctx.plan.effects()
    );
}

/// Regression test for carina#1683: data source input attributes that
/// reference another resource must be resolved against current state
/// *before* being passed to `read_data_source_with_retry`. Without
/// resolution the provider receives a debug-formatted `ResourceRef`
/// string and ships it to the remote API as a literal.
#[test]
fn resolve_data_source_refs_replaces_resource_ref_with_concrete_value() {
    use carina_core::resource::{AccessPath, DataSource};

    let identity_store_id = "d-9067c29a4b";

    // Managed resource with a binding — phase 1 would have refreshed it.
    let mut sso = Resource::with_provider("awscc", "sso.Instance", "carina-rs", None);
    sso.binding = Some("sso".to_string());

    // Data source referencing `sso.identity_store_id`. carina#3181:
    // data sources are a distinct typestate.
    let mut mizzy = DataSource::with_provider("aws", "identitystore.user", "mizzy", None);
    mizzy.attributes.insert(
        "identity_store_id".to_string(),
        Value::Deferred(DeferredValue::ResourceRef {
            path: AccessPath::new("sso", "identity_store_id"),
        }),
    );
    mizzy.attributes.insert(
        "user_name".to_string(),
        Value::Concrete(ConcreteValue::String("gosukenator@gmail.com".into())),
    );

    // Managed current state after phase 1: sso has been refreshed and its
    // state carries the concrete identity_store_id.
    let mut current_states: HashMap<ResourceId, State> = HashMap::new();
    let sso_state = State::existing(
        sso.id.clone(),
        HashMap::from([(
            "identity_store_id".to_string(),
            Value::Concrete(ConcreteValue::String(identity_store_id.into())),
        )]),
    );
    current_states.insert(sso.id.clone(), sso_state);

    let empty_registry = carina_core::schema::SchemaRegistry::new();
    let resolved = resolve_data_source_refs_for_refresh(
        &[sso],
        &[],
        &[mizzy],
        &crate::managed_current_from_states(current_states.clone()),
        &HashMap::new(),
        &empty_registry,
        &[],
    )
    .expect("resolution should succeed");

    assert_eq!(resolved.len(), 1, "only the data source should be returned");
    let DataSourceRefreshResolution::Resolved(resolved_mizzy) = &resolved[0] else {
        panic!("current-state-backed data source must be refreshable: {resolved:?}");
    };
    assert_eq!(
        resolved_mizzy.get_attr("identity_store_id"),
        Some(&Value::Concrete(ConcreteValue::String(
            identity_store_id.into()
        ))),
        "identity_store_id should be resolved to the concrete state value, \
         not a ResourceRef"
    );
    assert_eq!(
        resolved_mizzy.get_attr("user_name"),
        Some(&Value::Concrete(ConcreteValue::String(
            "gosukenator@gmail.com".into()
        ))),
        "literal inputs should pass through untouched"
    );
}

#[test]
fn resolve_data_source_refs_for_refresh_defers_unresolved_apply_time_input() {
    use carina_core::resource::{AccessPath, DataSource};

    let mut target = Resource::with_provider("test", "Upstream", "target", None);
    target.binding = Some("target".to_string());
    target.set_attr(
        "name",
        Value::Concrete(ConcreteValue::String("target".into())),
    );

    let mut lookup = DataSource::with_provider("test", "Lookup", "roles", None);
    lookup.attributes.insert(
        "filter".to_string(),
        Value::Deferred(DeferredValue::ResourceRef {
            path: AccessPath::new("target", "generated"),
        }),
    );

    let empty_registry = carina_core::schema::SchemaRegistry::new();
    let resolved = resolve_data_source_refs_for_refresh(
        &[target],
        &[],
        &[lookup],
        &HashMap::new(),
        &HashMap::new(),
        &empty_registry,
        &[],
    )
    .expect("resolution should classify deferred refs");

    assert_eq!(resolved.len(), 1);
    let DataSourceRefreshResolution::DeferredToApply {
        resource,
        unresolved,
    } = &resolved[0]
    else {
        panic!("unresolved input must be deferred, got {resolved:?}");
    };
    assert_eq!(resource.id.resource_type, "Lookup");
    assert_eq!(unresolved.len(), 1);
    assert_eq!(unresolved[0].attribute, "filter");
    assert_eq!(unresolved[0].paths[0].to_dot_string(), "target.generated");
}

#[test]
fn resolve_data_source_refs_for_refresh_defers_bare_binding_ref_to_new_resource() {
    use carina_core::resource::DataSource;

    let mut target = Resource::with_provider("test", "Upstream", "target", None);
    target.binding = Some("target".to_string());

    let mut lookup = DataSource::with_provider("test", "Lookup", "roles", None);
    lookup.attributes.insert(
        "filter".to_string(),
        Value::Deferred(DeferredValue::BindingRef {
            binding: "target".to_string(),
        }),
    );

    let empty_registry = carina_core::schema::SchemaRegistry::new();
    let resolved = resolve_data_source_refs_for_refresh(
        &[target],
        &[],
        &[lookup],
        &HashMap::new(),
        &HashMap::new(),
        &empty_registry,
        &[],
    )
    .expect("resolution should classify bare binding refs");

    let DataSourceRefreshResolution::DeferredToApply { unresolved, .. } = &resolved[0] else {
        panic!("bare BindingRef to a new resource must defer: {resolved:?}");
    };
    assert_eq!(unresolved[0].attribute, "filter");
    assert_eq!(unresolved[0].bindings, vec!["target".to_string()]);
}

#[test]
fn resolve_data_source_refs_for_refresh_defers_unknown_input_instead_of_calling_provider() {
    use carina_core::resource::{DataSource, UnknownReason};

    let mut lookup = DataSource::with_provider("test", "Lookup", "roles", None);
    lookup.attributes.insert(
        "filter".to_string(),
        Value::Deferred(DeferredValue::Unknown(UnknownReason::ForValue)),
    );

    let empty_registry = carina_core::schema::SchemaRegistry::new();
    let resolved = resolve_data_source_refs_for_refresh(
        &[],
        &[],
        &[lookup],
        &HashMap::new(),
        &HashMap::new(),
        &empty_registry,
        &[],
    )
    .expect("resolution should classify unknown inputs");

    let DataSourceRefreshResolution::DeferredToApply { unresolved, .. } = &resolved[0] else {
        panic!("Unknown input must not be treated as provider-ready: {resolved:?}");
    };
    assert_eq!(unresolved[0].attribute, "filter");
    assert_eq!(unresolved[0].unknowns, vec![UnknownReason::ForValue]);
}

#[test]
fn resolve_data_source_refs_for_refresh_defers_resolvable_interpolation_when_target_is_new() {
    use carina_core::resource::{AccessPath, DataSource, InterpolationPart};

    let mut target = Resource::with_provider("test", "Upstream", "target", None);
    target.binding = Some("target".to_string());
    target.set_attr(
        "role_name",
        Value::Concrete(ConcreteValue::String("target-role".into())),
    );

    let mut lookup = DataSource::with_provider("test", "Lookup", "roles", None);
    lookup.attributes.insert(
        "name_regex".to_string(),
        Value::Deferred(DeferredValue::Interpolation(vec![
            InterpolationPart::Literal("^".into()),
            InterpolationPart::Expr(Value::Deferred(DeferredValue::ResourceRef {
                path: AccessPath::new("target", "role_name"),
            })),
            InterpolationPart::Literal("$".into()),
        ])),
    );

    let empty_registry = carina_core::schema::SchemaRegistry::new();
    let resolved = resolve_data_source_refs_for_refresh(
        &[target],
        &[],
        &[lookup],
        &HashMap::new(),
        &HashMap::new(),
        &empty_registry,
        &[],
    )
    .expect("resolution should classify new-target input as deferred");

    let DataSourceRefreshResolution::DeferredToApply { unresolved, .. } = &resolved[0] else {
        panic!("value-resolvable input that references a new resource must defer: {resolved:?}");
    };
    assert_eq!(unresolved[0].paths[0].to_dot_string(), "target.role_name");
}

#[test]
fn resolve_data_source_refs_for_refresh_defers_folded_input_with_missing_dependency() {
    use carina_core::resource::DataSource;

    let mut target = Resource::with_provider("test", "Upstream", "registry_publish.target", None);
    target.binding = Some("registry_publish.target".to_string());

    // A resolved input may be folded to a concrete string while retaining
    // its structural dependency. Module expansion must carry that dependency
    // under the same instance-prefixed binding used by the managed target.
    let mut lookup = DataSource::with_provider("test", "Lookup", "registry_publish.roles", None);
    lookup.binding = Some("registry_publish.roles".to_string());
    lookup.attributes.insert(
        "filter".to_string(),
        Value::Concrete(ConcreteValue::String("^target$".into())),
    );
    lookup
        .dependency_bindings
        .insert("registry_publish.target".to_string());

    let resolved = resolve_data_source_refs_for_refresh(
        &[target],
        &[],
        &[lookup],
        &HashMap::new(),
        &HashMap::new(),
        &carina_core::schema::SchemaRegistry::new(),
        &[],
    )
    .expect("resolution should classify the preserved dependency");

    assert!(
        matches!(
            resolved[0],
            DataSourceRefreshResolution::DeferredToApply { .. }
        ),
        "a folded input whose dependency is not in state must defer: {resolved:?}"
    );
}

#[test]
fn resolve_data_source_refs_for_refresh_does_not_treat_missing_create_only_readback_as_replace() {
    use carina_core::resource::{AccessPath, DataSource};

    let mut target = Resource::with_provider("test", "Role", "target", None);
    target.binding = Some("target".to_string());
    target.set_attr(
        "role_name",
        Value::Concrete(ConcreteValue::String("same-name".into())),
    );

    let mut lookup = DataSource::with_provider("test", "Lookup", "roles", None);
    lookup.attributes.insert(
        "filter".to_string(),
        Value::Deferred(DeferredValue::ResourceRef {
            path: AccessPath::new("target", "role_name"),
        }),
    );

    let mut current_states = HashMap::new();
    current_states.insert(
        target.id.clone(),
        State::existing(target.id.clone(), HashMap::new()).with_identifier("old-id"),
    );
    let mut registry = carina_core::schema::SchemaRegistry::new();
    registry.insert(
        "test",
        (ResourceSchema::new("Role").attribute(
            AttributeSchema::optional("role_name", AttributeType::string())
                .create_only()
                .build(),
        ))
        .validate()
        .expect("schema fixture must be valid"),
    );

    let resolved = resolve_data_source_refs_for_refresh(
        &[target],
        &[],
        &[lookup],
        &crate::managed_current_from_states(current_states.clone()),
        &HashMap::new(),
        &registry,
        &[],
    )
    .expect("resolution should not infer replacement from a missing read-back value");

    assert!(
        matches!(resolved[0], DataSourceRefreshResolution::Resolved(_)),
        "an existing resource with a missing read-back create-only attr should refresh, not defer: {resolved:?}"
    );
}

#[test]
fn resolve_data_source_refs_for_refresh_defers_when_any_input_references_new_resource() {
    use carina_core::resource::{AccessPath, DataSource};

    let mut existing = Resource::with_provider("test", "Upstream", "existing", None);
    existing.binding = Some("existing".to_string());
    let mut target = Resource::with_provider("test", "Upstream", "target", None);
    target.binding = Some("target".to_string());

    let mut lookup = DataSource::with_provider("test", "Lookup", "roles", None);
    lookup.attributes.insert(
        "existing_filter".to_string(),
        Value::Deferred(DeferredValue::ResourceRef {
            path: AccessPath::new("existing", "name"),
        }),
    );
    lookup.attributes.insert(
        "new_filter".to_string(),
        Value::Deferred(DeferredValue::ResourceRef {
            path: AccessPath::new("target", "name"),
        }),
    );

    let mut current_states = HashMap::new();
    current_states.insert(
        existing.id.clone(),
        State::existing(
            existing.id.clone(),
            HashMap::from([(
                "name".to_string(),
                Value::Concrete(ConcreteValue::String("existing".into())),
            )]),
        ),
    );

    let empty_registry = carina_core::schema::SchemaRegistry::new();
    let resolved = resolve_data_source_refs_for_refresh(
        &[existing, target],
        &[],
        &[lookup],
        &crate::managed_current_from_states(current_states.clone()),
        &HashMap::new(),
        &empty_registry,
        &[],
    )
    .expect("resolution should classify mixed existing/new refs");

    assert!(
        matches!(
            resolved[0],
            DataSourceRefreshResolution::DeferredToApply { .. }
        ),
        "any new-resource input must defer the whole read: {resolved:?}"
    );
}

#[test]
fn resolve_data_source_refs_for_refresh_defers_read_chain_from_new_resource() {
    use carina_core::resource::{AccessPath, DataSource};

    let mut target = Resource::with_provider("test", "Upstream", "target", None);
    target.binding = Some("target".to_string());

    let mut first = DataSource::with_provider("test", "Lookup", "first", None);
    first.binding = Some("first".to_string());
    first.attributes.insert(
        "filter".to_string(),
        Value::Deferred(DeferredValue::ResourceRef {
            path: AccessPath::new("target", "name"),
        }),
    );

    let mut second = DataSource::with_provider("test", "Lookup", "second", None);
    second.binding = Some("second".to_string());
    second.attributes.insert(
        "filter".to_string(),
        Value::Deferred(DeferredValue::ResourceRef {
            path: AccessPath::new("first", "names"),
        }),
    );

    let empty_registry = carina_core::schema::SchemaRegistry::new();
    let resolved = resolve_data_source_refs_for_refresh(
        &[target],
        &[],
        &[first, second],
        &HashMap::new(),
        &HashMap::new(),
        &empty_registry,
        &[],
    )
    .expect("resolution should propagate deferred read chains");

    assert!(
        resolved.iter().all(|resolution| matches!(
            resolution,
            DataSourceRefreshResolution::DeferredToApply { .. }
        )),
        "read B must defer when it depends on deferred read A: {resolved:?}"
    );
}

#[test]
fn resolve_data_source_refs_for_refresh_reads_existing_resource_even_when_replaced() {
    use carina_core::resource::{AccessPath, DataSource};

    let mut target = Resource::with_provider("test", "Role", "target", None);
    target.binding = Some("target".to_string());
    target.set_attr(
        "role_name",
        Value::Concrete(ConcreteValue::String("new-name".into())),
    );

    let mut lookup = DataSource::with_provider("test", "Lookup", "roles", None);
    lookup.attributes.insert(
        "filter".to_string(),
        Value::Deferred(DeferredValue::ResourceRef {
            path: AccessPath::new("target", "arn"),
        }),
    );

    let mut current_states = HashMap::new();
    current_states.insert(
        target.id.clone(),
        State::existing(
            target.id.clone(),
            HashMap::from([
                (
                    "role_name".to_string(),
                    Value::Concrete(ConcreteValue::String("old-name".into())),
                ),
                (
                    "arn".to_string(),
                    Value::Concrete(ConcreteValue::String("old-arn".into())),
                ),
            ]),
        )
        .with_identifier("old-id"),
    );
    let mut registry = carina_core::schema::SchemaRegistry::new();
    registry.insert(
        "test",
        (ResourceSchema::new("Role")
            .attribute(
                AttributeSchema::optional("role_name", AttributeType::string())
                    .create_only()
                    .build(),
            )
            .attribute(
                AttributeSchema::provider_populated("arn", AttributeType::string()).build(),
            ))
        .validate()
        .expect("schema fixture must be valid"),
    );

    let resolved = resolve_data_source_refs_for_refresh(
        &[target],
        &[],
        &[lookup],
        &crate::managed_current_from_states(current_states.clone()),
        &HashMap::new(),
        &registry,
        &[],
    )
    .expect("resolution should classify refs to existing resources");

    // The refresh classifier is intentionally existence-only. Predicting
    // whether the differ will replace an existing upstream requires duplicating
    // differ semantics for provider defaults, absent attributes, and removable
    // fields. Refreshing the pre-replace state is ordinary plan-time staleness;
    // apply can re-read only for reads deferred because an upstream did not
    // exist at refresh time.
    assert!(
        matches!(resolved[0], DataSourceRefreshResolution::Resolved(_)),
        "read must refresh when the referenced binding already exists in state: {resolved:?}"
    );
}

#[tokio::test]
async fn literal_input_data_source_refresh_reads_provider_once() {
    use carina_core::resource::DataSource;

    let mut lookup = DataSource::with_provider("test", "Lookup", "roles", None);
    lookup.attributes.insert(
        "filter".to_string(),
        Value::Concrete(ConcreteValue::String("literal".into())),
    );

    let empty_registry = carina_core::schema::SchemaRegistry::new();
    let resolved = resolve_data_source_refs_for_refresh(
        &[],
        &[],
        &[lookup],
        &HashMap::new(),
        &HashMap::new(),
        &empty_registry,
        &[],
    )
    .expect("literal inputs should be refreshable");

    let provider = ReadWithRetryProvider::new(ReadBehavior::NotFound);
    let bindings = ResolvedBindings::default();
    let module_gate = carina_core::executor::ModuleConstraintGate::from_live_compositions(&[]);
    for resolution in resolved {
        match resolution {
            DataSourceRefreshResolution::Resolved(resource) => {
                read_data_source_with_retry(
                    &provider,
                    &resource,
                    &bindings,
                    &module_gate,
                    &[],
                    &empty_registry,
                )
                .await
                .expect("literal input read should succeed");
            }
            DataSourceRefreshResolution::DeferredToApply { .. } => {
                panic!("literal input read must not be deferred");
            }
        }
    }

    assert_eq!(
        provider.data_source_read_calls(),
        1,
        "literal-input data source should be read exactly once during refresh",
    );
}

#[tokio::test]
async fn invalid_data_source_preparation_keeps_complete_typed_diagnostic() {
    use carina_core::resource::DataSource;

    let lookup = DataSource::with_provider("test", "Lookup", "roles", None).with_attribute(
        "filter",
        Value::Concrete(ConcreteValue::String("bad".into())),
    );
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "test",
        (ResourceSchema::new("Lookup")
            .attribute(
                AttributeSchema::optional(
                    "filter",
                    AttributeType::refined_string(None, Some("^good-".to_string()), None, None),
                )
                .build(),
            )
            .as_data_source())
        .validate()
        .expect("schema fixture must be valid"),
    );
    let provider = ReadWithRetryProvider::new(ReadBehavior::NotFound);
    let module_gate = carina_core::executor::ModuleConstraintGate::from_live_compositions(&[]);

    let error = read_data_source_with_retry(
        &provider,
        &lookup,
        &ResolvedBindings::default(),
        &module_gate,
        &[],
        &schemas,
    )
    .await
    .expect_err("invalid data-source input must fail preparation");

    assert_eq!(
        error.to_string(),
        "[Lookup.roles] test.Lookup.roles: value constraint failed before provider dispatch: Invalid value 'bad' for 'filter': does not match required pattern /^good-/"
    );
    let AppError::ProviderPreparation(preparation) = &error else {
        panic!("typed preparation error was erased: {error:?}");
    };
    let carina_core::executor::ProviderPreparationError::ValueConstraint { errors, .. } =
        preparation.preparation_error()
    else {
        panic!("typed schema error was erased: {error:?}");
    };
    assert!(!errors.errors().is_empty());
    assert_eq!(
        error
            .to_string()
            .matches("does not match required pattern /^good-/")
            .count(),
        1
    );
    assert!(std::error::Error::source(&error).is_none());
    assert_eq!(provider.data_source_read_calls(), 0);
}

// Two resources with unknown types must surface as two distinct
// `AppError::Validation` entries instead of one joined string, so
// the driver can accumulate diagnostics across validators.
#[test]
fn validate_resources_with_ctx_returns_each_error_as_app_error() {
    let ctx = WiringContext::new_for_test(vec![], &[]);

    // Empty provider string sidesteps the "unknown provider, skip"
    // escape hatch (`known_providers` is empty), so each bad resource
    // produces its own "Unknown resource type" entry.
    let r1 = Resource::new("foo.nothing", "first");
    let r2 = Resource::new("bar.nothing", "second");
    let mut parsed = ParsedFile::default();
    parsed.resources = vec![r1, r2];

    let provider_ctx = ctx.provider_context();
    let errors = validate_resources_with_ctx(&ctx, &parsed, provider_ctx);
    assert_eq!(errors.len(), 2, "got {errors:?}");
    for err in &errors {
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
    }
}

// Smoke test for the dependency-chain wrappers: empty input
// exercises each wrapper and pins the `Vec<AppError>` return
// type. A regression back to `Result` fails to compile here.
#[test]
fn dependency_chain_wrappers_return_vec_app_error() {
    let ctx = WiringContext::new_for_test(vec![], &[]);
    let mut resources: Vec<Resource> = Vec::new();
    let providers: Vec<ProviderConfig> = Vec::new();

    let errors = resolve_attr_prefixes_with_ctx(&ctx, &mut resources);
    assert!(errors.is_empty(), "resolve_attr_prefixes: got {errors:?}");

    let canonical_resources =
        carina_core::value::canonicalize_resources_with_schemas(&mut resources, ctx.schemas())
            .unwrap();
    let errors = compute_anonymous_identifiers_with_ctx(&ctx, canonical_resources, &providers);
    assert!(
        errors.is_empty(),
        "compute_anonymous_identifiers: got {errors:?}",
    );
}

#[test]
fn paired_block_name_resolution_covers_resources_and_deferred_templates() {
    let schema = ResourceSchema::new("thing")
        .attribute(
            AttributeSchema::optional(
                "rules",
                AttributeType::list(AttributeType::struct_(
                    "Rule",
                    vec![StructField::optional("action", AttributeType::string()).build()],
                )),
            )
            .with_block_name("rule")
            .build(),
        )
        .validate()
        .expect("block-name fixture schema must be valid");
    let ctx = WiringContext::new_for_test_with_additional_schemas(
        Vec::new(),
        &[],
        [("test".to_string(), schema)],
    );
    let rule = Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
        ConcreteValue::Map(IndexMap::from([(
            "action".to_string(),
            Value::Concrete(ConcreteValue::String("allow".into())),
        )])),
    )]));
    let resource = Resource::with_provider("test", "thing", "direct", None)
        .with_attribute("rule", rule.clone());
    let template_resource = Resource::with_provider("test", "thing", "deferred", None)
        .with_attribute("rule", rule.clone());
    let deferred = carina_core::parser::DeferredForExpression {
        file: Some("main.crn".to_string()),
        line: 4,
        header: "for item in pending.items".to_string(),
        resource_type: "test.thing".to_string(),
        attributes: vec![("rule".to_string(), rule)],
        binding_name: "items".to_string(),
        iterable: Value::resource_ref("pending", "items", Vec::new()),
        binding: carina_core::parser::ForBinding::Simple("item".to_string()),
        template_resource,
    };
    let mut evaluated = carina_core::parser::ParsedFile::default();
    evaluated.resources = vec![resource];
    evaluated.deferred_for_expressions = vec![deferred];
    let unresolved = evaluated.clone();

    let canonical_unresolved = resolve_config_names_with_ctx(
        &ValidationPipelineAccess::new(),
        &ctx,
        evaluated,
        carina_core::config_loader::UnresolvedConfigSnapshot::from_parsed(unresolved),
    )
    .expect("paired block aliases must resolve");

    assert!(
        canonical_unresolved.parsed().resources[0]
            .attributes
            .contains_key("rules")
    );
    assert!(
        !canonical_unresolved.parsed().resources[0]
            .attributes
            .contains_key("rule")
    );
    let evaluated_deferred = &canonical_unresolved.parsed().deferred_for_expressions[0];
    assert!(
        evaluated_deferred
            .template_resource
            .attributes
            .contains_key("rules")
    );
    assert_eq!(evaluated_deferred.attributes[0].0, "rules");

    let unresolved_resources = canonical_unresolved.unresolved();
    assert!(unresolved_resources[0].attributes.contains_key("rules"));
    assert!(!unresolved_resources[0].attributes.contains_key("rule"));
}

#[test]
fn paired_block_name_resolution_reports_shared_conflict_once() {
    let schema = ResourceSchema::new("thing")
        .attribute(
            AttributeSchema::optional(
                "rules",
                AttributeType::list(AttributeType::struct_(
                    "Rule",
                    vec![StructField::optional("action", AttributeType::string()).build()],
                )),
            )
            .with_block_name("rule")
            .build(),
        )
        .validate()
        .expect("block-name fixture schema must be valid");
    let ctx = WiringContext::new_for_test_with_additional_schemas(
        Vec::new(),
        &[],
        [("test".to_string(), schema)],
    );
    let rules = Value::Concrete(ConcreteValue::List(Vec::new()));
    let resource = Resource::with_provider("test", "thing", "direct", None)
        .with_attribute("rule", rules.clone())
        .with_attribute("rules", rules);
    let mut evaluated = carina_core::parser::ParsedFile::default();
    evaluated.resources = vec![resource];
    let unresolved = evaluated.clone();

    let errors = resolve_config_names_with_ctx(
        &ValidationPipelineAccess::new(),
        &ctx,
        evaluated,
        carina_core::config_loader::UnresolvedConfigSnapshot::from_parsed(unresolved),
    )
    .expect_err("mixed block and attribute syntax must fail");

    assert_eq!(errors.len(), 1);
    assert_eq!(
        errors[0].to_string(),
        "test.thing.direct: cannot use both 'rule' and 'rules' (they refer to the same attribute)"
    );
}

#[test]
fn paired_name_resolution_retains_post_alias_prefix_changes_in_owned_file() {
    let schema = ResourceSchema::new("thing")
        .attribute(AttributeSchema::optional("name", AttributeType::string()).build())
        .validate()
        .expect("name fixture schema must be valid");
    let ctx = WiringContext::new_for_test_with_additional_schemas(
        Vec::new(),
        &[],
        [("test".to_string(), schema)],
    );
    let mut evaluated = carina_core::parser::ParsedFile::default();
    evaluated.resources = vec![
        Resource::with_provider("test", "thing", "direct", None).with_attribute(
            "name_prefix",
            Value::Concrete(ConcreteValue::String("owned-".into())),
        ),
    ];
    let unresolved = evaluated.clone();

    let canonical = resolve_config_names_with_ctx(
        &ValidationPipelineAccess::new(),
        &ctx,
        evaluated,
        carina_core::config_loader::UnresolvedConfigSnapshot::from_parsed(unresolved),
    )
    .expect("paired name resolution must succeed");

    let resource = &canonical.parsed().resources[0];
    assert_eq!(resource.prefixes.get("name"), Some(&"owned-".to_string()));
    assert!(!resource.attributes.contains_key("name_prefix"));
    assert!(matches!(
        resource.attributes.get("name"),
        Some(Value::Concrete(ConcreteValue::String(name)))
            if name.expose_plaintext().starts_with("owned-")
    ));
}

fn prefix_reconciliation_resource(attributes: &[&str]) -> Resource {
    let mut resource = Resource::with_provider("mock", "test.resource", "example", None);
    for attribute in attributes {
        resource
            .prefixes
            .insert((*attribute).to_string(), format!("{attribute}-prefix-"));
        resource.set_attr(
            (*attribute).to_string(),
            Value::Concrete(ConcreteValue::String(
                format!("generated-{attribute}-fallback").into(),
            )),
        );
    }
    resource
}

fn prefix_reconciliation_state_file(
    attributes: HashMap<String, Value>,
    partial_read: Option<carina_core::resource::PartialReadMarker>,
    prefixes: &[&str],
) -> StateFile {
    let id = ResourceId::with_provider_identity("mock", "test.resource", "example", None);
    let mut provider_state = State::existing(id, attributes).with_identifier("provider-id");
    provider_state.partial_read = partial_read;
    let mut row = carina_state::ResourceState::from_provider_state(
        "test.resource",
        "example",
        "mock",
        &provider_state,
    )
    .expect("prefix fixture state is persistable");
    for attribute in prefixes {
        row.prefixes
            .insert((*attribute).to_string(), format!("{attribute}-prefix-"));
    }
    let mut state_file = StateFile::new();
    state_file
        .upsert_resource(row)
        .expect("prefix fixture row is unique");
    state_file
}

fn reconcile_prefixes_from_refreshed_current(
    resource: &mut Resource,
    state_file: &StateFile,
    current: Current,
) {
    let managed_current = HashMap::from([(resource.id.clone(), current)]);
    let prefix_state_info = refreshed_prefix_state_info(
        std::slice::from_ref(resource),
        &Some(state_file.clone()),
        &managed_current,
    );
    identifier::reconcile_prefixed_names(
        std::slice::from_mut(resource),
        &|provider, resource_type, identity| {
            prefix_state_info
                .get(&prefix_state_key(provider, resource_type, identity))
                .cloned()
        },
    );
}

fn assert_plain_prefix_value(resource: &Resource, attribute: &str, expected: &str) {
    assert!(
        matches!(
            resource.attributes.get(attribute),
            Some(Value::Concrete(ConcreteValue::String(value)))
                if value.expose_plaintext() == expected
        ),
        "expected {attribute:?} to be {expected:?}, got {:?}",
        resource.attributes.get(attribute),
    );
}

#[test]
fn refreshed_prefix_reconciliation_keeps_stored_name_when_current_is_absent() {
    let mut resource = prefix_reconciliation_resource(&["name"]);
    let state_file = prefix_reconciliation_state_file(
        HashMap::from([(
            "name".to_string(),
            Value::Concrete(ConcreteValue::String("stored-name".into())),
        )]),
        None,
        &["name"],
    );

    reconcile_prefixes_from_refreshed_current(
        &mut resource,
        &state_file,
        Current::absent(ResourceId::with_provider_identity(
            "mock",
            "test.resource",
            "example",
            None,
        )),
    );

    assert_plain_prefix_value(&resource, "name", "stored-name");
}

#[test]
fn refreshed_prefix_reconciliation_keeps_stored_name_when_current_omits_it() {
    let mut resource = prefix_reconciliation_resource(&["name"]);
    let state_file = prefix_reconciliation_state_file(
        HashMap::from([(
            "name".to_string(),
            Value::Concrete(ConcreteValue::String("stored-name".into())),
        )]),
        None,
        &["name"],
    );
    let id = resource.id.clone();
    let current =
        Current::from_state(State::existing(id, HashMap::new()).with_identifier("provider-id"));

    reconcile_prefixes_from_refreshed_current(&mut resource, &state_file, current);

    assert_plain_prefix_value(&resource, "name", "stored-name");
}

#[test]
fn refreshed_prefix_reconciliation_prefers_current_marker_then_current_value() {
    let pending = carina_core::resource::PendingReRead::incomplete("provider omitted name");
    let marker = carina_core::resource::PartialReadMarker::new(
        pending.clone(),
        BTreeSet::from(["name".to_string()]),
    )
    .expect("fixture marker covers one attribute");
    let state_file = prefix_reconciliation_state_file(
        HashMap::from([(
            "name".to_string(),
            Value::Concrete(ConcreteValue::String("stored-name".into())),
        )]),
        None,
        &["name"],
    );
    let id = ResourceId::with_provider_identity("mock", "test.resource", "example", None);
    let mut current_state =
        State::existing(id.clone(), HashMap::new()).with_identifier("provider-id");
    current_state.partial_read = Some(marker);
    let mut marked_resource = prefix_reconciliation_resource(&["name"]);

    reconcile_prefixes_from_refreshed_current(
        &mut marked_resource,
        &state_file,
        Current::from_state(current_state),
    );

    assert!(matches!(
        marked_resource.attributes.get("name"),
        Some(Value::Deferred(DeferredValue::Unknown(
            carina_core::resource::UnknownReason::PendingReRead(actual)
        ))) if actual == &pending
    ));

    let mut valued_resource = prefix_reconciliation_resource(&["name"]);
    let current_with_value = Current::from_state(
        State::existing(
            id,
            HashMap::from([(
                "name".to_string(),
                Value::Concrete(ConcreteValue::String("refreshed-name".into())),
            )]),
        )
        .with_identifier("provider-id"),
    );

    reconcile_prefixes_from_refreshed_current(
        &mut valued_resource,
        &state_file,
        current_with_value,
    );

    assert_plain_prefix_value(&valued_resource, "name", "refreshed-name");
}

#[test]
fn pre_refresh_prefix_reconciliation_uses_persisted_value_and_marker() {
    let pending = carina_core::resource::PendingReRead::read_failed("read failed");
    let marker = carina_core::resource::PartialReadMarker::new(
        pending.clone(),
        BTreeSet::from(["alias".to_string()]),
    )
    .expect("fixture marker covers one attribute");
    let state_file = prefix_reconciliation_state_file(
        HashMap::from([(
            "name".to_string(),
            Value::Concrete(ConcreteValue::String("stored-name".into())),
        )]),
        Some(marker),
        &["name", "alias"],
    );
    let mut resources = vec![prefix_reconciliation_resource(&["name", "alias"])];

    reconcile_prefixed_names(&mut resources, &Some(state_file));

    assert_plain_prefix_value(&resources[0], "name", "stored-name");
    assert!(matches!(
        resources[0].attributes.get("alias"),
        Some(Value::Deferred(DeferredValue::Unknown(
            carina_core::resource::UnknownReason::PendingReRead(actual)
        ))) if actual == &pending
    ));
}

struct RegionIdentityFactory;

impl carina_core::provider::ProviderFactory for RegionIdentityFactory {
    fn name(&self) -> &str {
        "awscc"
    }

    fn display_name(&self) -> &str {
        "AWSCC region identity test provider"
    }

    fn provider_config_attribute_types(&self) -> carina_core::schema::ValidatedConfigSchema {
        carina_core::schema::ValidatedConfigSchema::validate(BTreeMap::from([(
            "region".to_string(),
            carina_core::schema::AttributeType::enum_(
                carina_core::schema::TypeIdentity::new(Some("aws"), Vec::<String>::new(), "Region"),
                Some(vec!["ap-northeast-1".to_string()]),
                vec![("ap-northeast-1".to_string(), "ap_northeast_1".to_string())],
                None,
                None,
            ),
        )]))
        .expect("test provider config schema must be valid")
    }

    fn validate_config(
        &self,
        _attributes: &carina_core::provider::CanonicalProviderConfigAttributes,
    ) -> carina_core::provider::ProviderResult<()> {
        Ok(())
    }

    fn extract_region(&self, _attributes: &IndexMap<String, Value>) -> String {
        "ap-northeast-1".to_string()
    }

    fn create_provider(
        &self,
        _binding: Option<&str>,
        _config: &carina_core::provider::ProviderReadyConfig,
    ) -> carina_core::provider::BoxFuture<
        '_,
        carina_core::provider::ProviderResult<Box<dyn carina_core::provider::Provider>>,
    > {
        Box::pin(async {
            Ok(Box::new(MockProvider::new()) as Box<dyn carina_core::provider::Provider>)
        })
    }

    fn create_normalizer(
        &self,
        _binding: Option<&str>,
        _config: &carina_core::provider::ProviderReadyConfig,
    ) -> carina_core::provider::BoxFuture<
        '_,
        carina_core::provider::ProviderResult<Box<dyn carina_core::provider::ProviderNormalizer>>,
    > {
        Box::pin(async {
            Ok(Box::new(carina_core::provider::NoopNormalizer)
                as Box<dyn carina_core::provider::ProviderNormalizer>)
        })
    }

    fn schemas(&self) -> Vec<carina_core::schema::ValidatedResourceSchema> {
        let schema_drafts: Vec<carina_core::schema::ResourceSchema> = {
            vec![
                carina_core::schema::ResourceSchema::new("ec2.Route").attribute(
                    carina_core::schema::AttributeSchema::optional(
                        "route_table_id",
                        carina_core::schema::AttributeType::string(),
                    )
                    .build(),
                ),
            ]
        };
        schema_drafts
            .into_iter()
            .map(|schema| {
                schema
                    .validate()
                    .expect("factory test schema must be valid")
            })
            .collect()
    }

    fn identity_attributes(&self) -> Vec<&str> {
        vec!["region"]
    }
}

fn region_identity_ctx() -> WiringContext {
    WiringContext::new_for_test(vec![Box::new(RegionIdentityFactory)], &[])
}

fn region_provider_config(raw_region: &str) -> ProviderConfig {
    ProviderConfig {
        name: "awscc".to_string(),
        attributes: indexmap::indexmap! {
            "region".to_string() => Value::Concrete(ConcreteValue::enum_identifier(raw_region)),
        },
        default_tags: IndexMap::new(),
        source: None,
        version: None,
        revision: None,
        unresolved_attributes: IndexMap::new(),
        binding: None,
    }
}

fn anonymous_route_resource() -> Resource {
    let mut resource = Resource::pending_with_provider("awscc", "ec2.Route", None);
    resource.set_attr(
        "route_table_id".to_string(),
        Value::Concrete(ConcreteValue::String("rtb-123".into())),
    );
    resource
}

#[test]
fn late_anonymous_identity_resolution_reports_all_unavailable_provider_schemas() {
    let ctx = WiringContext::new_for_test(vec![], &[]);
    let (mut resources, _) = build_override_aware_pair_for_test(
        canonical_unresolved(
            vec![
                Resource::pending_with_provider("aws", "s3.Bucket", None),
                Resource::pending_with_provider("aws", "s3.Bucket", None),
                Resource::pending_with_provider("awscc", "ec2.Vpc", Some("secondary".to_string())),
            ],
            ctx.schemas(),
        ),
        None::<&StateFile>,
        LivePreApplyInputs {
            managed: &[],
            compositions: &[],
            managed_current: &HashMap::new(),
            data_source_values: &DataSourceValues::empty(),
            remote_bindings: &HashMap::new(),
            wait_aliases: &[],
        },
    )
    .expect("schema-less test resource has no references to resolve");
    let claims = StateBlockClaims::empty();
    let mut addressed = AddressedState {
        managed_current: HashMap::new(),
        orphans: Orphans::new(),
        saved_attrs: crate::empty_lifted_saved_attrs(),
        prev_explicit: HashMap::new(),
        moved_pairs: Vec::new(),
    };

    let result = reconcile_late_anonymous_identities(
        &ctx,
        LateAnonymousIdentityInputs {
            resources: &mut resources,
            addressed: &mut addressed,
            data_sources: &[],
            state_file: None,
            state_block_claims: &claims,
            providers: &[],
        },
    );

    let error = match result {
        Ok(_) => panic!("schema-less anonymous resource must not receive a fallback identity"),
        Err(error) => error,
    };
    assert!(matches!(error, AppError::Config(_)), "got {error:?}");
    assert_eq!(
        error.to_string(),
        "2 anonymous aws.s3.Bucket resources need provider 'aws', which is not declared or could not be loaded; declare provider 'aws' so its schema is available\n\
         anonymous awscc.ec2.Vpc resource needs provider instance 'secondary' of 'awscc', which is not declared or could not be loaded; declare provider 'awscc' so its schema is available",
    );
    assert!(
        resources
            .resources()
            .iter()
            .all(|resource| resource.id.identity_str().is_none()),
        "the error path must leave every anonymous identity pending",
    );
}

#[test]
fn late_anonymous_reconciliation_sees_early_move_target_and_leaves_other_orphan() {
    use carina_state::state::StateFile;

    let ctx = WiringContext::new_for_test(vec![Box::new(AssociationCreateOnlyFactory)], &[]);
    let desired = Resource::with_provider(
        "awscc",
        "ec2.SubnetRouteTableAssociation",
        test_identity("fresh_late_identity"),
        None,
    )
    .with_attribute(
        "route_table_id",
        Value::Concrete(ConcreteValue::String("rtb-private".into())),
    )
    .with_attribute(
        "subnet_id",
        Value::Concrete(ConcreteValue::String("subnet-a".into())),
    );
    let (mut resources, _) = build_override_aware_pair_for_test(
        canonical_unresolved(vec![desired], ctx.schemas()),
        None::<&StateFile>,
        LivePreApplyInputs {
            managed: &[],
            compositions: &[],
            managed_current: &HashMap::new(),
            data_source_values: &DataSourceValues::empty(),
            remote_bindings: &HashMap::new(),
            wait_aliases: &[],
        },
    )
    .expect("late-reconciliation fixture has no unresolved live references");

    let early_source_name = "ec2_subnet_route_table_association_11111111";
    let other_orphan_name = "ec2_subnet_route_table_association_22222222";
    let mut state_file = StateFile::new();
    state_file
        .upsert_resource(association_state(
            early_source_name,
            "rtb-private",
            "subnet-a",
        ))
        .expect("test state setup must be valid");
    state_file
        .upsert_resource(association_state(
            other_orphan_name,
            "rtb-private",
            "subnet-b",
        ))
        .expect("test state setup must be valid");

    let desired_id = ResolvedResourceId::new(resources.resources()[0].id.clone());
    let early_source_id = ResolvedResourceId::with_provider_identity(
        "awscc",
        "ec2.SubnetRouteTableAssociation",
        test_identity(early_source_name),
        None,
    );
    let other_orphan_id = ResolvedResourceId::with_provider_identity(
        "awscc",
        "ec2.SubnetRouteTableAssociation",
        test_identity(other_orphan_name),
        None,
    );
    let desired_ids = HashSet::from([desired_id.as_inner().clone()]);
    let mut addressed = AddressedState {
        managed_current: HashMap::new(),
        orphans: state_file.build_orphans(&desired_ids).into_plan_orphans(),
        saved_attrs: crate::empty_lifted_saved_attrs(),
        prev_explicit: HashMap::new(),
        moved_pairs: Vec::new(),
    };
    let claims = StateBlockClaims::empty();
    let state_view = AnonymousReconciliationStateView::new(&ctx, &state_file, &[]);
    let early_moves = reconcile_anonymous_identifiers_with_ctx(
        &ctx,
        resources.resources(),
        &[],
        &state_view,
        &claims,
    );
    assert_eq!(
        detected_move_identity_pairs(&early_moves),
        vec![(
            early_source_name.to_string(),
            desired_id.identity_str().to_string(),
        )],
        "the unique full match must win over the partial-match orphan",
    );
    apply_detected_resource_moves(
        &mut addressed,
        early_moves,
        Some(&state_file),
        MoveOverlapPolicy::ComposeForward,
    )
    .expect("early move must re-key its orphan state");
    let early_pair = (early_source_id, desired_id.clone());
    assert_eq!(addressed.moved_pairs, vec![early_pair.clone()]);
    assert!(addressed.orphans.contains_key(other_orphan_id.as_inner()));

    reconcile_late_anonymous_identities(
        &ctx,
        LateAnonymousIdentityInputs {
            resources: &mut resources,
            addressed: &mut addressed,
            data_sources: &[],
            state_file: Some(&state_file),
            state_block_claims: &claims,
            providers: &[],
        },
    )
    .expect("late anonymous identity must resolve");

    assert_eq!(
        addressed.moved_pairs,
        vec![early_pair],
        "the projected early destination must make late reconciliation a no-op"
    );
    assert!(
        addressed.orphans.contains_key(other_orphan_id.as_inner()),
        "the other matching row must remain an orphan and therefore a Delete candidate",
    );
}

#[test]
fn compute_anonymous_identifiers_with_ctx_canonicalizes_provider_config_identity_enums() {
    let ctx = region_identity_ctx();
    let providers_awscc = vec![region_provider_config("awscc.Region.ap_northeast_1")];
    let providers_aws = vec![region_provider_config("aws.Region.ap_northeast_1")];

    let mut resources_awscc = vec![anonymous_route_resource()];
    let mut resources_aws = vec![anonymous_route_resource()];
    let canonical_awscc = carina_core::value::canonicalize_resources_with_schemas(
        &mut resources_awscc,
        ctx.schemas(),
    )
    .unwrap();
    let errors = compute_anonymous_identifiers_with_ctx(&ctx, canonical_awscc, &providers_awscc);
    assert!(errors.is_empty(), "awscc spelling errors: {errors:?}");
    let canonical_aws =
        carina_core::value::canonicalize_resources_with_schemas(&mut resources_aws, ctx.schemas())
            .unwrap();
    let errors = compute_anonymous_identifiers_with_ctx(&ctx, canonical_aws, &providers_aws);
    assert!(errors.is_empty(), "aws spelling errors: {errors:?}");

    assert_eq!(
        resources_awscc[0]
            .id
            .identity_str()
            .expect("resolved identity"),
        resources_aws[0]
            .id
            .identity_str()
            .expect("resolved identity"),
        "provider config region spelling must canonicalize before anonymous hash"
    );
}

#[test]
fn detected_anonymous_to_named_moves_canonicalize_provider_config_identity_enums() {
    use carina_state::state::{ResourceState, StateFile};

    let ctx = region_identity_ctx();
    let providers_awscc = vec![region_provider_config("awscc.Region.ap_northeast_1")];
    let providers_aws = vec![region_provider_config("aws.Region.ap_northeast_1")];

    let mut anonymous = vec![anonymous_route_resource()];
    let canonical_anonymous =
        carina_core::value::canonicalize_resources_with_schemas(&mut anonymous, ctx.schemas())
            .unwrap();
    let errors =
        compute_anonymous_identifiers_with_ctx(&ctx, canonical_anonymous, &providers_awscc);
    assert!(errors.is_empty(), "anonymous setup errors: {errors:?}");
    let old_name = anonymous[0]
        .id
        .identity_str()
        .expect("resolved identity")
        .to_string();

    let mut named = anonymous_route_resource();
    named.id = ResourceId::with_provider_identity("awscc", "ec2.Route", "route", None);
    named.binding = Some("route".to_string());
    let resources = vec![named.clone()];

    let mut state_file = StateFile::new();
    state_file
        .upsert_resource(
            ResourceState::try_applied("ec2.Route", test_identity(&old_name), "awscc", "route-123")
                .expect("fixture identifier is non-empty"),
        )
        .expect("test state setup must be valid");
    let desired_ids = HashSet::from([named.id.clone()]);
    let mut addressed = AddressedState {
        managed_current: HashMap::new(),
        orphans: state_file.build_orphans(&desired_ids).into_plan_orphans(),
        prev_explicit: HashMap::new(),
        saved_attrs: crate::empty_lifted_saved_attrs(),
        moved_pairs: Vec::new(),
    };
    let state_view = AnonymousReconciliationStateView::new(&ctx, &state_file, &[]);
    let moves = detect_anonymous_to_named_moves_with_ctx(
        &ctx,
        &resources,
        &providers_aws,
        &state_view,
        &carina_core::identifier::StateBlockClaims::empty(),
    )
    .unwrap();
    apply_detected_resource_moves(
        &mut addressed,
        moves,
        Some(&state_file),
        MoveOverlapPolicy::ComposeForward,
    )
    .unwrap();

    assert_eq!(
        addressed.moved_pairs,
        vec![(
            ResolvedResourceId::with_provider_identity(
                "awscc",
                "ec2.Route",
                test_identity(&old_name),
                None,
            ),
            ResolvedResourceId::new(named.id.clone())
        )],
        "provider config region spelling must canonicalize before rename simhash"
    );
    assert!(addressed.orphans.is_empty());
    let current = addressed
        .managed_current
        .get(&named.id)
        .expect("renamed state");
    let state = match current {
        Current::Absent(state) => state.state(),
        Current::Present(present) => present.state(),
        Current::MissingIdentifier(state) => state.state(),
        Current::CreateInFlight(pending) => {
            panic!("renamed test state unexpectedly remained in flight: {pending:?}")
        }
    };
    assert_eq!(state.identifier.as_deref(), Some("route-123"));
}

// Smoke test for the provider/composition wrappers and the typed module-call
// checker on empty input.
#[test]
fn module_checker_and_provider_wrappers_accept_empty_input() {
    let ctx = WiringContext::new_for_test(vec![], &[]);
    let parsed = ParsedFile::default();
    let provider_ctx = ctx.provider_context();

    let errors = validate_provider_region_with_ctx(&ctx, &parsed);
    assert!(errors.is_empty(), "provider_region: got {errors:?}");

    let module_signatures = carina_core::module_resolver::ResolvedModuleSignatures::new();
    let binding_use_report = carina_core::validation::check_bare_binding_uses(
        &parsed,
        &carina_core::validation::ValidationTypeContext::new(
            &parsed,
            ctx.schemas(),
            provider_ctx,
            &module_signatures,
        ),
    );
    let bindings = carina_core::binding_index::BindingIndex::from_parsed_with_module_signatures(
        &parsed,
        ctx.schemas(),
        &module_signatures,
    );
    let findings = carina_core::validation::check_module_calls(
        carina_core::validation::ModuleCallCheckInput {
            authored: &parsed,
            semantic: Some(&parsed),
            base_dir: Some(std::path::Path::new(".")),
            imported_signatures: &module_signatures,
            provider_context: provider_ctx,
            schemas: ctx.schemas(),
            binding_use_report: &binding_use_report,
            bindings: &bindings,
            reference_checks: carina_core::validation::ModuleCallReferenceChecks::Enabled,
        },
    );
    assert!(findings.is_empty(), "module_calls: got {findings:?}");

    let errors = validate_composition_ref_types_with_ctx(&ctx, &parsed);
    assert!(errors.is_empty(), "composition_ref_types: got {errors:?}");
}

// =====================================================================
// Empty `${}` interpolation rejection (#2487)
//
// The parser accepts `${}` mid-edit (#2480) and the LSP surfaces it as
// a per-location warning. CLI validate / plan / apply must reject it
// explicitly so a buffer with literal `${}` can't reach a provider and
// surface only at the AWS edge.
// =====================================================================

fn parsed_with_attr(attr_name: &str, attr_value: Value) -> ParsedFile {
    let mut r = Resource::new("foo.bar", "x");
    r.attributes.insert(attr_name.to_string(), attr_value);
    let mut parsed = ParsedFile::default();
    parsed.resources = vec![r];
    parsed
}

#[test]
fn validate_rejects_top_level_empty_interpolation() {
    use carina_core::resource::{DeferredValue, InterpolationPart, UnknownReason, Value};

    let value = Value::Deferred(DeferredValue::Interpolation(vec![
        InterpolationPart::Literal("arn:aws:iam::".into()),
        InterpolationPart::Expr(Value::Deferred(DeferredValue::Unknown(
            UnknownReason::EmptyInterpolation,
        ))),
        InterpolationPart::Literal(":root".into()),
    ]));
    let parsed = parsed_with_attr("aws", value);

    let errors = validate_no_empty_interpolations(&parsed);
    assert_eq!(errors.len(), 1, "expected one error, got: {:?}", errors);
    let msg = match &errors[0] {
        AppError::Validation(s) => s,
        other => panic!("expected AppError::Validation, got {:?}", other),
    };
    assert!(
        msg.contains("empty interpolation"),
        "message must mention 'empty interpolation'; got: {}",
        msg
    );
    assert!(
        msg.contains("foo.bar.x"),
        "message must include the resource id (provider.type.name); got: {}",
        msg
    );
    assert!(
        msg.contains("aws"),
        "message must name the offending attribute; got: {}",
        msg
    );
}

#[test]
fn validate_rejects_empty_interpolation_inside_secret() {
    use carina_core::resource::{DeferredValue, InterpolationPart, UnknownReason, Value};

    let inner = Value::Deferred(DeferredValue::Interpolation(vec![InterpolationPart::Expr(
        Value::Deferred(DeferredValue::Unknown(UnknownReason::EmptyInterpolation)),
    )]));
    let parsed = parsed_with_attr(
        "password",
        Value::Deferred(DeferredValue::Secret(Box::new(inner))),
    );

    let errors = validate_no_empty_interpolations(&parsed);
    assert_eq!(
        errors.len(),
        1,
        "empty `${{}}` wrapped in `secret(...)` must error; got: {:?}",
        errors
    );
}

#[test]
fn validate_rejects_empty_interpolation_in_either_coalesce_operand() {
    use carina_core::resource::{ConcreteValue, DeferredValue, UnknownReason, Value};

    let bad = || Value::Deferred(DeferredValue::Unknown(UnknownReason::EmptyInterpolation));
    let good = || Value::Concrete(ConcreteValue::String("fallback".into()));

    for value in [
        Value::Deferred(DeferredValue::Coalesce {
            value: Box::new(bad()),
            default: Box::new(good()),
        }),
        Value::Deferred(DeferredValue::Coalesce {
            value: Box::new(good()),
            default: Box::new(bad()),
        }),
    ] {
        let errors = validate_no_empty_interpolations(&parsed_with_attr("name", value));
        assert_eq!(
            errors.len(),
            1,
            "both coalesce operands must be checked for empty interpolation",
        );
    }
}

#[test]
fn validate_rejects_empty_interpolation_inside_function_call() {
    use carina_core::resource::{
        ConcreteValue, DeferredValue, InterpolationPart, UnknownReason, Value,
    };

    let bad = Value::Deferred(DeferredValue::Interpolation(vec![InterpolationPart::Expr(
        Value::Deferred(DeferredValue::Unknown(UnknownReason::EmptyInterpolation)),
    )]));
    let fn_call = Value::Deferred(DeferredValue::FunctionCall {
        name: carina_core::builtins::BuiltinFunction::join,
        args: vec![
            Value::Concrete(ConcreteValue::String("-".into())).into(),
            bad.into(),
        ],
        source_line: 0.into(),
    });
    let parsed = parsed_with_attr("name", fn_call);

    let errors = validate_no_empty_interpolations(&parsed);
    assert_eq!(
        errors.len(),
        1,
        "empty `${{}}` inside a function-call arg must error; got: {:?}",
        errors
    );
}

#[test]
fn validate_rejects_empty_interpolation_nested_in_map() {
    use carina_core::resource::{
        ConcreteValue, DeferredValue, InterpolationPart, UnknownReason, Value,
    };
    use indexmap::IndexMap;

    let inner = Value::Deferred(DeferredValue::Interpolation(vec![
        InterpolationPart::Literal("prefix-".into()),
        InterpolationPart::Expr(Value::Deferred(DeferredValue::Unknown(
            UnknownReason::EmptyInterpolation,
        ))),
    ]));
    let mut map = IndexMap::new();
    map.insert("key".to_string(), inner);
    let parsed = parsed_with_attr("tags", Value::Concrete(ConcreteValue::Map(map)));

    let errors = validate_no_empty_interpolations(&parsed);
    assert_eq!(
        errors.len(),
        1,
        "nested-in-map empty `${{}}` must error; got: {:?}",
        errors
    );
}

#[test]
fn validate_rejects_empty_interpolation_nested_in_list() {
    use carina_core::resource::{
        ConcreteValue, DeferredValue, InterpolationPart, UnknownReason, Value,
    };

    let inner = Value::Deferred(DeferredValue::Interpolation(vec![InterpolationPart::Expr(
        Value::Deferred(DeferredValue::Unknown(UnknownReason::EmptyInterpolation)),
    )]));
    let parsed = parsed_with_attr("items", Value::Concrete(ConcreteValue::List(vec![inner])));

    let errors = validate_no_empty_interpolations(&parsed);
    assert_eq!(
        errors.len(),
        1,
        "nested-in-list empty `${{}}` must error; got: {:?}",
        errors
    );
}

#[test]
fn validate_rejects_empty_interpolation_in_export_value() {
    use carina_core::resource::{DeferredValue, InterpolationPart, UnknownReason, Value};

    let bad = Value::Deferred(DeferredValue::Interpolation(vec![InterpolationPart::Expr(
        Value::Deferred(DeferredValue::Unknown(UnknownReason::EmptyInterpolation)),
    )]));
    let mut parsed = parse(
        "exports { url = 'placeholder' }\n",
        &carina_core::parser::ProviderContext::default(),
    )
    .expect("authored export fixture must parse");
    parsed.export_params[0].value = Some(bad);

    let errors = validate_no_empty_interpolations(&parsed);
    assert_eq!(
        errors.len(),
        1,
        "empty `${{}}` in `exports {{ ... }}` value must error; got: {:?}",
        errors
    );
    let msg = match &errors[0] {
        AppError::Validation(s) => s,
        _ => panic!("not a Validation error"),
    };
    assert!(
        msg.contains("exports") && msg.contains("url"),
        "message must name the offending export; got: {}",
        msg
    );
}

#[test]
fn validate_rejects_empty_interpolation_in_attribute_param_default() {
    use carina_core::resource::{DeferredValue, InterpolationPart, UnknownReason, Value};

    let bad = Value::Deferred(DeferredValue::Interpolation(vec![InterpolationPart::Expr(
        Value::Deferred(DeferredValue::Unknown(UnknownReason::EmptyInterpolation)),
    )]));
    let mut parsed = parse(
        "attributes { region = 'placeholder' }\n",
        &carina_core::parser::ProviderContext::default(),
    )
    .expect("authored attribute fixture must parse");
    parsed.attribute_params[0].value = Some(bad);

    let errors = validate_no_empty_interpolations(&parsed);
    assert_eq!(
        errors.len(),
        1,
        "empty `${{}}` in `attributes {{ ... }}` default must error; got: {:?}",
        errors
    );
}

#[test]
fn validate_passes_when_no_empty_interpolation() {
    use carina_core::resource::{ConcreteValue, DeferredValue, InterpolationPart, Value};

    // Non-empty interpolation: must not error.
    let value = Value::Deferred(DeferredValue::Interpolation(vec![
        InterpolationPart::Literal("prefix-".into()),
        InterpolationPart::Expr(Value::Concrete(ConcreteValue::String("real-value".into()))),
    ]));
    let parsed = parsed_with_attr("aws", value);

    let errors = validate_no_empty_interpolations(&parsed);
    assert!(
        errors.is_empty(),
        "non-empty interpolation must pass; got: {:?}",
        errors
    );
}

// carina#3913: read_with_retry accepts only a validated provider identifier,
// so a caller cannot accidentally turn a missing identifier into an
// identity-less provider read.
#[cfg(test)]
mod read_with_retry_identifier_tests {
    use super::*;
    use carina_core::provider::{ProviderResult, ReadRequest};
    use carina_core::resource::State;
    use futures::future::BoxFuture;
    use std::sync::Mutex;

    /// A provider whose `read` records every invocation. Used to verify
    /// that `read_with_retry` does *not* call into the provider when
    /// `identifier` is None.
    struct RecordingProvider {
        calls: Mutex<Vec<(ResourceId, String)>>,
    }

    impl RecordingProvider {
        fn new() -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
            }
        }
    }

    impl Provider for RecordingProvider {
        fn name(&self) -> &str {
            "recording"
        }

        fn read(
            &self,
            id: &ResourceId,
            identifier: &AppliedIdentifier,
            _request: ReadRequest,
        ) -> BoxFuture<'_, ProviderResult<ReadOutcome>> {
            self.calls
                .lock()
                .unwrap()
                .push((id.clone(), identifier.as_str().to_string()));
            let id = id.clone();
            let identifier = identifier.clone();
            Box::pin(async move {
                Ok(ReadOutcome::found(
                    AppliedState::new(id, identifier.as_str(), HashMap::new())
                        .expect("typed read identifier is non-empty"),
                ))
            })
        }

        fn read_data_source(
            &self,
            resource: &carina_core::provider::ProviderReadyDataSource,
        ) -> BoxFuture<'_, ProviderResult<State>> {
            let id = resource.id.clone();
            Box::pin(async move { Ok(State::not_found(id)) })
        }

        fn create(
            &self,
            _id: &ResourceId,
            _request: carina_core::provider::CreateRequest,
            attempt: carina_core::provider::CreateAttempt,
        ) -> BoxFuture<
            '_,
            Result<carina_core::provider::CreateOutcome, carina_core::provider::CreateFailure>,
        > {
            Box::pin(async move {
                Ok(attempt
                    .issued(
                        AppliedIdentifier::new("recording-id")
                            .expect("fixture identifier is non-empty"),
                        Sent::AllRequestAttributes,
                    )
                    .succeeded(HashMap::new()))
            })
        }

        unexpected_resume_create!();

        fn update(
            &self,
            id: &ResourceId,
            identifier: &carina_core::provider::AppliedIdentifier,
            _request: carina_core::provider::UpdateRequest,
        ) -> BoxFuture<'_, ProviderResult<carina_core::provider::UpdateOutcome>> {
            let id = id.clone();
            let identifier = identifier.to_string();
            Box::pin(async move {
                Ok(carina_core::provider::UpdateOutcome::Success {
                    state: AppliedState::new(id, identifier, HashMap::new())
                        .expect("provider input identifier is non-empty"),
                })
            })
        }

        fn delete(
            &self,
            _id: &ResourceId,
            _identifier: &carina_core::provider::AppliedIdentifier,
            _request: carina_core::provider::DeleteRequest,
        ) -> BoxFuture<'_, ProviderResult<()>> {
            Box::pin(async { Ok(()) })
        }

        fn required_permissions(
            &self,
            _id: &ResourceId,
            _op: carina_core::effect::PlanOp,
        ) -> ProviderResult<Vec<String>> {
            Ok(Vec::new())
        }
    }

    #[tokio::test]
    async fn read_with_retry_forwards_validated_identifier() {
        let provider = RecordingProvider::new();
        let id = ResourceId::with_provider_identity("awscc", "iam.Role", "existing", None);
        let identifier = crate::fixture_applied_identifier("AROABC123");

        let _state = read_with_retry(&provider, &id, &identifier).await.unwrap();

        let calls = provider.calls.lock().unwrap();
        assert_eq!(calls.len(), 1, "provider.read must be called exactly once");
        assert_eq!(calls[0].1, "AROABC123");
    }
}

// ---------------------------------------------------------------------
// carina#3132 PR-1: `expand_same_config_deferred_for` — the pure
// post-refresh expansion the plan path calls. Tested with a hand-built
// post-refresh managed current state (the documented refresh-phase output);
// `create_plan_from_parsed_with_upstream_with_ctx` invokes this exact function,
// so this is the real plan-path expansion, not a transform in
// isolation (cf. MEMORY "unit-test path ≠ apply path").
//
// `--refresh=false` needs no dedicated case here: the pure function is
// refresh-agnostic (it expands against whatever managed-current map it is
// given — cached-state and live-refresh are the same input shape, both
// covered below). The only refresh-specific code is the child-state
// restore in `create_plan_from_parsed_with_upstream_with_ctx`, which routes the
// new children through the *same* `sf.build_state_for_resource` /
// `State::not_found` path the original resources already use under
// `--refresh=false` (via the shared `children()` filter).
// ---------------------------------------------------------------------
#[test]
fn deferred_template_binding_match_requires_single_numeric_index() {
    assert!(binding_matches_deferred_template(
        "validation_records[0]",
        "validation_records"
    ));
    assert!(binding_matches_deferred_template(
        "validation_records[42]",
        "validation_records"
    ));
    assert!(!binding_matches_deferred_template(
        "validation_records[]",
        "validation_records"
    ));
    assert!(!binding_matches_deferred_template(
        "validation_records[abc]",
        "validation_records"
    ));
    assert!(!binding_matches_deferred_template(
        "validation_records[0][1]",
        "validation_records"
    ));
}

fn deferred_replace_test_template() -> carina_core::parser::DeferredForExpression {
    let template_resource =
        Resource::with_provider("aws", "route53.Record", "validation_records", None)
            .with_binding("validation_records");

    carina_core::parser::DeferredForExpression {
        file: Some("main.crn".to_string()),
        line: 12,
        header: "for opt in cert.domain_validation_options".to_string(),
        resource_type: "aws.route53.Record".to_string(),
        attributes: vec![],
        binding_name: "validation_records".to_string(),
        iterable: Value::resource_ref("cert", "domain_validation_options", Vec::new()),
        binding: carina_core::parser::ForBinding::Simple("opt".to_string()),
        template_resource,
    }
}

fn deferred_replace_test_target() -> DeferredCreateTarget {
    DeferredCreateTarget {
        id: ResolvedResourceId::with_provider_identity(
            "aws",
            "__deferred_for",
            "validation_records",
            None,
        ),
        template: deferred_replace_test_template(),
    }
}

fn delete_effect_for_binding(binding: &str) -> Effect {
    Effect::Delete {
        id: ResolvedResourceId::with_provider_identity(
            "aws",
            "route53.Record",
            test_identity(binding),
            None,
        ),
        identifier: crate::fixture_applied_identifier(format!("{binding}-old-id")),
        generation: carina_core::effect::EffectGeneration::Current,
        directives: Directives::default(),
        binding: Some(binding.to_string()),
        dependencies: std::collections::HashSet::from(["cert".to_string()]),
        explicit_dependencies: std::collections::HashSet::new(),
        blocked_by_updates: std::collections::HashSet::new(),
    }
}

#[test]
fn deferred_create_targets_absorb_matching_orphan_deletes_into_deferred_replace() {
    let target = deferred_replace_test_target();
    let mut plan = Plan::new();
    plan.add(delete_effect_for_binding("validation_records[0]"));

    add_deferred_create_effects(&mut plan, std::slice::from_ref(&target));

    let mut deferred_replaces = plan.effects().iter().filter_map(|effect| match effect {
        Effect::DeferredReplace(payload) => {
            Some((&payload.deletes, &payload.id, &payload.template))
        }
        _ => None,
    });
    let (deletes, id, template) = deferred_replaces
        .next()
        .expect("matching delete + deferred target must produce DeferredReplace");
    assert!(
        deferred_replaces.next().is_none(),
        "plan must contain exactly one DeferredReplace"
    );
    assert_eq!(id, &target.id);
    assert_eq!(
        template.root_bindings(),
        std::collections::BTreeSet::from(["cert".to_string()])
    );
    assert_eq!(template.binding_name, "validation_records");
    assert_eq!(deletes.len(), 1);
    assert_eq!(
        deletes[0].id,
        ResourceId::with_provider_identity("aws", "route53.Record", "validation_records[0]", None)
    );
    assert_eq!(deletes[0].binding.as_deref(), Some("validation_records[0]"));

    assert!(
        !plan.effects().iter().any(|effect| {
            matches!(
                effect,
                Effect::Delete {
                    binding: Some(binding),
                    ..
                } if binding == "validation_records[0]"
            )
        }),
        "absorbed delete must not remain as a standalone Delete"
    );
    assert!(
        !plan
            .effects()
            .iter()
            .any(|effect| matches!(effect, Effect::DeferredCreate { .. })),
        "absorbed target must not also remain as DeferredCreate"
    );
}

#[test]
fn deferred_create_targets_absorb_matching_map_key_orphan_deletes() {
    let target = deferred_replace_test_target();
    let mut plan = Plan::new();
    plan.add(delete_effect_for_binding("validation_records.alpha"));
    plan.add(delete_effect_for_binding(
        "validation_records['key with space']",
    ));
    plan.add(delete_effect_for_binding("validation_records['']"));

    add_deferred_create_effects(&mut plan, std::slice::from_ref(&target));

    let payload = plan
        .effects()
        .iter()
        .find_map(|effect| match effect {
            Effect::DeferredReplace(payload) => Some(payload),
            _ => None,
        })
        .expect("map-key deletes must be paired with their deferred create");
    assert_eq!(payload.deletes.len(), 3);
    assert!(
        !plan
            .effects()
            .iter()
            .any(|effect| matches!(effect, Effect::Delete { .. })),
        "stable map-key children must not remain standalone cleanups beside runtime upserts",
    );
}

#[test]
fn deferred_create_targets_do_not_absorb_deposed_deletes() {
    let target = deferred_replace_test_target();
    let deposed_key = carina_state::DeposedKey::new_unique();
    let mut deposed_delete = delete_effect_for_binding("validation_records[0]");
    if let Effect::Delete {
        generation,
        identifier,
        ..
    } = &mut deposed_delete
    {
        *generation = carina_core::effect::EffectGeneration::Deposed(deposed_key.clone());
        *identifier = crate::fixture_applied_identifier("deposed-old-id");
    } else {
        unreachable!("helper builds Delete");
    }
    let mut plan = Plan::new();
    plan.add(deposed_delete);

    add_deferred_create_effects(&mut plan, std::slice::from_ref(&target));

    assert!(
        plan.effects()
            .iter()
            .any(|effect| matches!(effect, Effect::DeferredCreate { .. })),
        "deposed delete must not turn the deferred create into DeferredReplace"
    );
    assert!(
        !plan
            .effects()
            .iter()
            .any(|effect| matches!(effect, Effect::DeferredReplace(_))),
        "deposed delete must not be absorbed into DeferredReplace"
    );
    assert!(
        plan.effects().iter().any(|effect| {
            matches!(
                effect,
                Effect::Delete {
                    binding: Some(binding),
                    generation: carina_core::effect::EffectGeneration::Deposed(key),
                    ..
                } if binding == "validation_records[0]" && key == &deposed_key
            )
        }),
        "deposed delete must remain top-level so remove-deposed writeback can observe it"
    );
}

#[test]
fn deferred_create_targets_do_not_absorb_unrelated_orphan_deletes() {
    let target = deferred_replace_test_target();
    let mut plan = Plan::new();
    plan.add(delete_effect_for_binding("some_other_resource[0]"));

    add_deferred_create_effects(&mut plan, std::slice::from_ref(&target));

    assert!(
        plan.effects()
            .iter()
            .any(|effect| matches!(effect, Effect::DeferredCreate { .. })),
        "unmatched target must stay a plain DeferredCreate"
    );
    assert!(
        plan.effects().iter().any(|effect| {
            matches!(
                effect,
                Effect::Delete {
                    binding: Some(binding),
                    ..
                } if binding == "some_other_resource[0]"
            )
        }),
        "unrelated orphan delete must stay a plain Delete"
    );
    assert!(
        !plan
            .effects()
            .iter()
            .any(|effect| matches!(effect, Effect::DeferredReplace(_))),
        "unrelated delete must not be falsely absorbed"
    );
}

mod expand_same_config_deferred_for_tests {
    use super::*;
    use carina_core::binding_index::WaitAliasSpec;
    use carina_core::parser::{ProviderContext, WarningKind};
    use carina_core::resource::{ConcreteValue, DeferredValue, State, UnknownReason, Value};
    use std::collections::{HashMap, HashSet};

    /// `let cert` (same-config) + a `for` over its provider-read
    /// `account_ids`, plus a plain independent resource so re-sort
    /// stability has something to preserve order against.
    ///
    /// The loop body uses the **bare** loop variable (`target = id`).
    /// Chained loop-var field access (`opt.resource_record.name`) is
    /// covered separately by
    /// `chained_loop_var_field_access_resolves_post_expansion` below
    /// (carina#3136).
    const SRC: &str = r#"
        let cert = aws.acm.Certificate {
            domain_name       = "registry.example.com"
            validation_method = "DNS"
        }

        aws.ec2.Vpc {
            name       = "anchor"
            cidr_block = "10.0.0.0/16"
        }

        for (_, id) in cert.account_ids {
            aws.sso.Assignment {
                instance_arn = "arn:aws:sso:::instance/ssoins-1"
                target_id    = id
                target_type  = "AWS_ACCOUNT"
            }
        }
    "#;

    /// Build managed current state as the refresh phase would: the parsed
    /// `cert` resource's own id → an existing State carrying the
    /// provider-read list attribute the loop iterates.
    fn states_with_cert(
        parsed: &ParsedFile,
        attr: &str,
        value: Value,
    ) -> HashMap<ResourceId, State> {
        let cert = parsed
            .resources
            .iter()
            .find(|r| r.binding.as_deref() == Some("cert"))
            .expect("parsed cert resource");
        let mut attrs = HashMap::new();
        attrs.insert(attr.to_string(), value);
        let mut states = HashMap::new();
        states.insert(
            cert.id.clone(),
            State::existing(cert.id.clone(), attrs).with_identifier("cert-id"),
        );
        states
    }

    fn account_ids() -> Value {
        Value::Concrete(ConcreteValue::List(vec![
            Value::Concrete(ConcreteValue::String("111111111111".into())),
            Value::Concrete(ConcreteValue::String("222222222222".into())),
        ]))
    }

    fn unknown_account_ids() -> Value {
        Value::Deferred(DeferredValue::Unknown(UnknownReason::ForValue))
    }

    #[test]
    fn deferred_for_warning_filter_preserves_single_quoted_warning_on_same_line() {
        let src = "for k in upstream.ids { awscc.s3.Bucket { name = 'a-${k}' } }\n";
        let parsed = parse(src, &ProviderContext::default()).expect("fixture parses");
        assert_eq!(parsed.deferred_for_expressions.len(), 1);
        assert!(
            parsed
                .warnings
                .iter()
                .any(|w| w.kind == WarningKind::SingleQuotedInterpolation),
            "fixture should carry the single-quoted interpolation-like warning"
        );

        let out = expand_same_config_deferred_for(
            &parsed,
            &[],
            &HashMap::new(),
            &DataSourceValues::empty(),
            &carina_core::schema::SchemaRegistry::new(),
            &carina_core::provider::RawSavedAttrs::default()
                .lift(&carina_core::schema::SchemaRegistry::new()),
            &HashMap::new(),
            &HashMap::new(),
            &[] as &[WaitAliasSpec],
            &HashSet::new(),
            &HashSet::new(),
        )
        .expect("deferred-for expansion should succeed");

        assert!(
            out.printed_warnings,
            "same-line deferred-for filtering must not delete unrelated parse warnings"
        );
    }

    #[test]
    fn saved_plan_expansion_suppresses_duplicate_warning_output_but_keeps_candidate_state() {
        let src = "for k in upstream.ids { awscc.s3.Bucket { name = 'a-${k}' } }\n";
        let mut parsed = parse(src, &ProviderContext::default()).expect("fixture parses");
        let state_file = None;
        let schemas = SchemaRegistry::new();
        let prelude = DeferredForExpansionPrelude::new_for_test(&parsed, &state_file, &schemas)
            .expect("build expansion prelude");
        let prepared = prelude
            .preserve_addresses(PreserveDeferredForExpansionInputs {
                managed_current: HashMap::new(),
                orphans: Orphans::new(),
                already_refreshed: HashSet::new(),
            })
            .seal(DataSourceValues::empty());

        let out = prepared
            .expand_parsed(
                &mut parsed,
                &schemas,
                &HashMap::new(),
                DeferredForWarningPolicy::Suppress,
            )
            .expect("deferred-for expansion should succeed");
        assert!(
            !out.printed_warnings,
            "verification must not print warnings"
        );

        let expanded = out.into_expanded_file(parsed);
        assert_eq!(
            expanded.deferred_for_expressions.len(),
            1,
            "a DeferredCreate template remains available for secret rehydration"
        );
        assert!(
            expanded
                .warnings
                .iter()
                .any(|warning| warning.kind == WarningKind::SingleQuotedInterpolation),
            "suppression controls rendering, not the candidate parser state"
        );
    }

    /// Direct unit test of `RefreshableChildIds::select` — the sole
    /// constructor of the post-expansion refresh iterator. The
    /// `expand_*` tests cover it end-to-end, but `select` carries the
    /// carina#3141 invariant ("yield exactly the ids in the set, skip
    /// everything else") and a refactor could break it without tripping
    /// the higher-level tests, so pin it directly. Builds a
    /// `RefreshableChildIds` from a known id subset and asserts `select`
    /// over a resource slice yields precisely those resources, in slice
    /// order, and nothing outside the set.
    #[test]
    fn refreshable_child_ids_select_yields_exactly_the_set() {
        // `Resource::new` gives each resource a distinct, concrete
        // `ResourceId` directly — no parse/ID-reconcile step, so the
        // test pins `select`'s set-membership logic in isolation
        // (anonymous ids are still pending right after `parse`, which is
        // a different concern covered by the expand_* tests).
        let r_a = Resource::new("aws.ec2.Vpc", "a");
        let r_b = Resource::new("aws.ec2.Vpc", "b");
        let r_c = Resource::new("aws.ec2.Vpc", "c");
        let resources = vec![r_a.clone(), r_b.clone(), r_c.clone()];

        // Refreshable set = {a, c}; b must be skipped.
        let mut ids = std::collections::HashSet::new();
        ids.insert(r_a.id.clone());
        ids.insert(r_c.id.clone());
        let refreshable = RefreshableChildIds(ids);

        assert_eq!(refreshable.len(), 2);
        assert!(!refreshable.is_empty());
        assert!(refreshable.contains(&r_a.id));
        assert!(!refreshable.contains(&r_b.id));

        let no_orphans = Orphans::new();
        let selected: Vec<&ResourceId> = refreshable
            .select(&resources, &no_orphans)
            .map(|r| &r.id)
            .collect();
        assert_eq!(
            selected,
            vec![&r_a.id, &r_c.id],
            "select must yield exactly the set members, in slice order, \
             and skip the resource not in the set"
        );

        // An empty set selects nothing — the "all expanded children are
        // moved targets" case from the plan/apply guard.
        let empty = RefreshableChildIds::default();
        assert!(empty.is_empty());
        assert_eq!(empty.select(&resources, &no_orphans).count(), 0);

        let orphan_state =
            State::existing(r_a.id.clone(), HashMap::new()).with_identifier("resource-a");
        let present = match Current::from_state(orphan_state) {
            Current::Absent(state) => panic!("test orphan is absent: {state:?}"),
            Current::Present(present) => present,
            Current::MissingIdentifier(state) => {
                panic!("test orphan lacks a provider identifier: {state:?}")
            }
            Current::CreateInFlight(pending) => {
                panic!("test orphan unexpectedly has an in-flight create: {pending:?}")
            }
        };
        let orphan_owned = Orphans::from([carina_core::differ::Orphan::present(
            present,
            Some("a".to_string()),
            Directives::default(),
        )]);
        let selected: Vec<&ResourceId> = refreshable
            .select(&resources, &orphan_owned)
            .map(|r| &r.id)
            .collect();
        assert_eq!(
            selected,
            vec![&r_c.id],
            "an orphan-owned child must not be duplicated into current states by refresh"
        );
    }

    #[test]
    fn same_config_read_iterable_materializes_in_plan_resources() {
        let parsed = parse(SRC, &ProviderContext::default()).expect("parse");
        assert_eq!(
            parsed.deferred_for_expressions.len(),
            1,
            "loop must be deferred at parse time (iterable is a same-config provider-read)"
        );
        let sorted = sort_resources_by_dependencies(&parsed.resources).unwrap();
        let states = states_with_cert(&parsed, "account_ids", account_ids());

        let out = expand_same_config_deferred_for(
            &parsed,
            &sorted,
            &crate::managed_current_from_states(states.clone()),
            &DataSourceValues::empty(),
            &carina_core::schema::SchemaRegistry::new(),
            &carina_core::provider::RawSavedAttrs::default()
                .lift(&carina_core::schema::SchemaRegistry::new()),
            &HashMap::new(),
            &HashMap::new(),
            &[] as &[WaitAliasSpec],
            &std::collections::HashSet::new(),
            &std::collections::HashSet::new(),
        )
        .expect("expand");

        let assignments: Vec<_> = out
            .sorted_resources
            .iter()
            .filter(|r| r.id.resource_type.contains("Assignment"))
            .collect();
        assert_eq!(
            assignments.len(),
            2,
            "one concrete Assignment per cert.account_ids entry; got {:?}",
            out.sorted_resources
                .iter()
                .map(|r| r.id.to_string())
                .collect::<Vec<_>>()
        );

        // The bare loop variable must be substituted from the
        // *refreshed* cert value — proving the post-refresh
        // ResolvedBindings view (not the empty pre-refresh map) fed the
        // expansion. This is the carina#3132 fix.
        let mut targets: Vec<String> = assignments
            .iter()
            .filter_map(|r| match r.get_attr("target_id") {
                Some(Value::Concrete(ConcreteValue::String(s))) => {
                    Some(s.expose_plaintext().to_owned())
                }
                _ => None,
            })
            .collect();
        targets.sort();
        assert_eq!(
            targets,
            vec!["111111111111".to_string(), "222222222222".to_string()],
            "target_id must be substituted from the refreshed \
             cert.account_ids entries"
        );

        assert!(
            out.residual_deferred_for.is_empty(),
            "resolved loop must leave no residual; got {:?}",
            out.residual_deferred_for
        );
        let expanded = out.into_expanded_file(parsed);
        assert!(
            expanded.deferred_for_expressions.is_empty(),
            "materialized loops must not remain in the candidate parser view"
        );
    }

    #[test]
    fn expand_same_config_emits_deferred_create_when_iterable_is_unresolved() {
        let parsed = parse(SRC, &ProviderContext::default()).expect("parse");
        let sorted = sort_resources_by_dependencies(&parsed.resources).unwrap();
        let states = states_with_cert(&parsed, "account_ids", unknown_account_ids());

        let out = expand_same_config_deferred_for(
            &parsed,
            &sorted,
            &crate::managed_current_from_states(states.clone()),
            &DataSourceValues::empty(),
            &carina_core::schema::SchemaRegistry::new(),
            &carina_core::provider::RawSavedAttrs::default()
                .lift(&carina_core::schema::SchemaRegistry::new()),
            &HashMap::new(),
            &HashMap::new(),
            &[] as &[WaitAliasSpec],
            &std::collections::HashSet::new(),
            &std::collections::HashSet::new(),
        )
        .expect("expand");

        assert_eq!(
            out.deferred_create_targets.len(),
            1,
            "one deferred-for expression should be carried for apply-time re-expansion"
        );
        assert_eq!(
            out.deferred_create_targets[0].template.root_bindings(),
            std::collections::BTreeSet::from(["cert".to_string()])
        );
        assert!(
            out.sorted_resources
                .iter()
                .all(|r| !r.id.resource_type.contains("Assignment")),
            "no Assignment may be pre-expanded from an unresolved iterable"
        );
        assert!(
            out.new_child_ids.is_empty(),
            "unresolved iterable must not report materialized children"
        );
        let expanded = out.into_expanded_file(parsed);
        assert_eq!(
            expanded.deferred_for_expressions.len(),
            1,
            "deferred effect templates must remain in the candidate parser view"
        );
    }

    #[test]
    fn expand_same_config_pre_expands_when_iterable_is_concrete() {
        let parsed = parse(SRC, &ProviderContext::default()).expect("parse");
        let sorted = sort_resources_by_dependencies(&parsed.resources).unwrap();
        let states = states_with_cert(&parsed, "account_ids", account_ids());

        let out = expand_same_config_deferred_for(
            &parsed,
            &sorted,
            &crate::managed_current_from_states(states.clone()),
            &DataSourceValues::empty(),
            &carina_core::schema::SchemaRegistry::new(),
            &carina_core::provider::RawSavedAttrs::default()
                .lift(&carina_core::schema::SchemaRegistry::new()),
            &HashMap::new(),
            &HashMap::new(),
            &[] as &[WaitAliasSpec],
            &std::collections::HashSet::new(),
            &std::collections::HashSet::new(),
        )
        .expect("expand");

        let assignments: Vec<_> = out
            .sorted_resources
            .iter()
            .filter(|r| r.id.resource_type.contains("Assignment"))
            .collect();
        assert_eq!(assignments.len(), 2);
        assert!(
            out.deferred_create_targets.is_empty(),
            "fully concrete iterable must pre-expand instead of emitting DeferredCreate"
        );
    }

    #[test]
    fn expand_same_config_passes_through_pr3558_dependency_bindings_on_pre_expanded_children() {
        let parsed = parse(SRC, &ProviderContext::default()).expect("parse");
        let sorted = sort_resources_by_dependencies(&parsed.resources).unwrap();
        let states = states_with_cert(&parsed, "account_ids", account_ids());

        let out = expand_same_config_deferred_for(
            &parsed,
            &sorted,
            &crate::managed_current_from_states(states.clone()),
            &DataSourceValues::empty(),
            &carina_core::schema::SchemaRegistry::new(),
            &carina_core::provider::RawSavedAttrs::default()
                .lift(&carina_core::schema::SchemaRegistry::new()),
            &HashMap::new(),
            &HashMap::new(),
            &[] as &[WaitAliasSpec],
            &std::collections::HashSet::new(),
            &std::collections::HashSet::new(),
        )
        .expect("expand");

        let assignments: Vec<_> = out
            .sorted_resources
            .iter()
            .filter(|r| r.id.resource_type.contains("Assignment"))
            .collect();
        assert_eq!(assignments.len(), 2);
        for assignment in assignments {
            assert!(
                assignment.dependency_bindings.contains("cert"),
                "pre-expanded child must retain the iterable binding dependency"
            );
        }
    }

    #[test]
    fn resort_preserves_pre_expansion_relative_order() {
        // carina#3132 highest-risk requirement: appending loop children
        // and re-sorting must not reorder already-planned resources
        // (SimHash / moved-matching stability).
        let parsed = parse(SRC, &ProviderContext::default()).expect("parse");
        let sorted = sort_resources_by_dependencies(&parsed.resources).unwrap();
        let pre_order: Vec<String> = sorted.iter().map(|r| r.id.to_string()).collect();
        let states = states_with_cert(&parsed, "account_ids", account_ids());

        let out = expand_same_config_deferred_for(
            &parsed,
            &sorted,
            &crate::managed_current_from_states(states.clone()),
            &DataSourceValues::empty(),
            &carina_core::schema::SchemaRegistry::new(),
            &carina_core::provider::RawSavedAttrs::default()
                .lift(&carina_core::schema::SchemaRegistry::new()),
            &HashMap::new(),
            &HashMap::new(),
            &[] as &[WaitAliasSpec],
            &std::collections::HashSet::new(),
            &std::collections::HashSet::new(),
        )
        .expect("expand");

        let post_filtered: Vec<String> = out
            .sorted_resources
            .iter()
            .map(|r| r.id.to_string())
            .filter(|id| pre_order.contains(id))
            .collect();
        assert_eq!(
            post_filtered, pre_order,
            "re-sort must preserve the relative order of already-planned resources"
        );
        // The two materialized Assignment children — and only those —
        // are reported as new (none of the pre-expansion ids leak in).
        assert_eq!(
            out.new_child_ids.len(),
            2,
            "new_child_ids must be exactly the materialized loop children"
        );
        assert!(
            out.new_child_ids
                .iter()
                .all(|id| !pre_order.contains(&id.to_string())),
            "new_child_ids must not include any pre-expansion resource"
        );
    }

    #[test]
    fn unresolvable_iterable_stays_deferred_no_misexpansion() {
        // No cert state → iterable genuinely unknowable. The loop must
        // stay in residual (carina#3128 placeholder), NOT mis-expand.
        let parsed = parse(SRC, &ProviderContext::default()).expect("parse");
        let sorted = sort_resources_by_dependencies(&parsed.resources).unwrap();
        let empty_states: HashMap<ResourceId, State> = HashMap::new();

        let out = expand_same_config_deferred_for(
            &parsed,
            &sorted,
            &crate::managed_current_from_states(empty_states.clone()),
            &DataSourceValues::empty(),
            &carina_core::schema::SchemaRegistry::new(),
            &carina_core::provider::RawSavedAttrs::default()
                .lift(&carina_core::schema::SchemaRegistry::new()),
            &HashMap::new(),
            &HashMap::new(),
            &[] as &[WaitAliasSpec],
            &std::collections::HashSet::new(),
            &std::collections::HashSet::new(),
        )
        .expect("expand");

        assert!(
            out.sorted_resources
                .iter()
                .all(|r| !r.id.resource_type.contains("Assignment")),
            "no Assignment may materialize without a resolvable iterable"
        );
        assert_eq!(
            out.deferred_create_targets.len(),
            1,
            "the unresolvable loop must become an apply-time re-expansion target"
        );
        assert!(
            out.residual_deferred_for.is_empty(),
            "apply-time re-expansion targets must not be mixed into residual deferred-for warnings"
        );
        assert_eq!(
            out.sorted_resources.len(),
            sorted.len(),
            "resource set unchanged when nothing materialized"
        );
    }

    #[test]
    fn upstream_state_iterable_still_expands_via_unified_view() {
        // Regression guard: the pre-existing `upstream_state` iterable
        // path must keep working now that it flows through the same
        // projected ResolvedBindings (design non-goal: must not regress
        // the upstream path).
        let src = r#"
            let orgs = upstream_state {
                source = "../organizations"
            }

            for account_id in orgs.accounts {
                awscc.sso.Assignment {
                    instance_arn = 'arn:aws:sso:::instance/ssoins-1'
                    target_id    = account_id
                    target_type  = 'AWS_ACCOUNT'
                }
            }
        "#;
        let parsed = parse(src, &ProviderContext::default()).expect("parse");
        assert_eq!(parsed.deferred_for_expressions.len(), 1);
        let sorted = sort_resources_by_dependencies(&parsed.resources).unwrap();

        let mut orgs_attrs = HashMap::new();
        orgs_attrs.insert(
            "accounts".to_string(),
            Value::Concrete(ConcreteValue::List(vec![
                Value::Concrete(ConcreteValue::String("111111111111".into())),
                Value::Concrete(ConcreteValue::String("222222222222".into())),
            ])),
        );
        let mut remote = HashMap::new();
        remote.insert("orgs".to_string(), orgs_attrs);

        let out = expand_same_config_deferred_for(
            &parsed,
            &sorted,
            &HashMap::new(),
            &DataSourceValues::empty(),
            &carina_core::schema::SchemaRegistry::new(),
            &carina_core::provider::RawSavedAttrs::default()
                .lift(&carina_core::schema::SchemaRegistry::new()),
            &HashMap::new(),
            &remote,
            &[] as &[WaitAliasSpec],
            &std::collections::HashSet::new(),
            &std::collections::HashSet::new(),
        )
        .expect("expand");

        let assignments = out
            .sorted_resources
            .iter()
            .filter(|r| r.id.resource_type.contains("Assignment"))
            .count();
        assert_eq!(
            assignments, 2,
            "upstream_state iterable must still expand via the unified view"
        );
        assert!(out.residual_deferred_for.is_empty());
    }

    #[test]
    fn chained_loop_var_field_access_resolves_post_expansion() {
        // carina#3136 (the flipped carina#3132 PR-1 limitation pin):
        // chained field access on the loop variable
        // (`opt.resource_record.name`) is parsed to
        // `Access(Unknown(ForValue), .resource_record.name)` and reduced against the
        // real element by `substitute_placeholder` at for-expansion.
        // The loop materializes AND its attributes resolve to concrete
        // values — this is the carina#3132 PR-3 real-registry shape.
        let src = r#"
            let cert = aws.acm.Certificate {
                domain_name       = "r.example.com"
                validation_method = "DNS"
            }

            for (_, opt) in cert.domain_validation_options {
                aws.route53.RecordSet {
                    name             = opt.resource_record.name
                    type             = "CNAME"
                    resource_records = [opt.resource_record.value]
                }
            }
        "#;
        let parsed = parse(src, &ProviderContext::default()).expect("parse");
        let sorted = sort_resources_by_dependencies(&parsed.resources).unwrap();

        let mut rr = indexmap::IndexMap::new();
        rr.insert(
            "name".to_string(),
            Value::Concrete(ConcreteValue::String("_a1.r.example.com".into())),
        );
        rr.insert(
            "value".to_string(),
            Value::Concrete(ConcreteValue::String("_a1.acm-validations.aws.".into())),
        );
        let mut entry = indexmap::IndexMap::new();
        entry.insert(
            "resource_record".to_string(),
            Value::Concrete(ConcreteValue::Map(rr)),
        );
        let dvo = Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
            ConcreteValue::Map(entry),
        )]));
        let states = states_with_cert(&parsed, "domain_validation_options", dvo);

        let out = expand_same_config_deferred_for(
            &parsed,
            &sorted,
            &crate::managed_current_from_states(states.clone()),
            &DataSourceValues::empty(),
            &carina_core::schema::SchemaRegistry::new(),
            &carina_core::provider::RawSavedAttrs::default()
                .lift(&carina_core::schema::SchemaRegistry::new()),
            &HashMap::new(),
            &HashMap::new(),
            &[] as &[WaitAliasSpec],
            &std::collections::HashSet::new(),
            &std::collections::HashSet::new(),
        )
        .expect("expand");

        let record_sets: Vec<_> = out
            .sorted_resources
            .iter()
            .filter(|r| r.id.resource_type.contains("RecordSet"))
            .collect();
        assert_eq!(
            record_sets.len(),
            1,
            "loop materializes one RecordSet per domain_validation_options entry"
        );
        // carina#3136: the chained loop-var ref is now substituted from
        // the real refreshed element — `name` is the concrete String,
        // not an unresolved ResourceRef.
        assert_eq!(
            record_sets[0].get_attr("name"),
            Some(&Value::Concrete(ConcreteValue::String(
                "_a1.r.example.com".into()
            ))),
            "name must resolve to the refreshed \
             cert.domain_validation_options[0].resource_record.name"
        );
        // And the nested access inside a list literal
        // (`resource_records = [opt.resource_record.value]`) resolves too.
        assert_eq!(
            record_sets[0].get_attr("resource_records"),
            Some(&Value::Concrete(ConcreteValue::List(vec![
                Value::Concrete(ConcreteValue::String("_a1.acm-validations.aws.".into()))
            ]))),
            "resource_records[0] must resolve to ...resource_record.value"
        );
    }

    /// The carina#3141 repro DSL: `let cert` + same-config
    /// `for (_, opt) in cert.domain_validation_options` loop. Builds the
    /// expansion once with NO moved targets, then again declaring the
    /// materialized child as a `moved` `to`. Asserts:
    ///   - `new_child_ids` is identical in both runs (the moved-exclusion
    ///     must not change what materialized).
    ///   - run 1: the child IS in `refreshable_child_ids` (non-moved
    ///     expanded children are still refreshed — regression guard
    ///     against an over-broad filter).
    ///   - run 2: the child is NOT in `refreshable_child_ids` (its
    ///     migrated state from `materialize_moved_states` must survive,
    ///     so the differ emits `Move`, never a same-address `Create`).
    ///
    /// Both the plan path (`create_plan_from_parsed_with_upstream_with_ctx`) and
    /// the apply path (`run_apply_locked`) refresh exactly
    /// `refreshable_child_ids` — this single typed field is what makes
    /// the moved-exclusion impossible to diverge between them
    /// (cf. MEMORY "unit-test path ≠ apply path"; the divergence is a
    /// compile error against `DeferredForExpansion`, not a runtime gap).
    #[test]
    fn moved_target_child_is_excluded_from_refreshable_set() {
        let src = r#"
            let cert = aws.acm.Certificate {
                domain_name       = "r.example.com"
                validation_method = "DNS"
            }

            for (_, opt) in cert.domain_validation_options {
                aws.route53.RecordSet {
                    name             = opt.resource_record.name
                    type             = "CNAME"
                    resource_records = [opt.resource_record.value]
                }
            }
        "#;
        let parsed = parse(src, &ProviderContext::default()).expect("parse");
        let sorted = sort_resources_by_dependencies(&parsed.resources).unwrap();

        let mut rr = indexmap::IndexMap::new();
        rr.insert(
            "name".to_string(),
            Value::Concrete(ConcreteValue::String("_a1.r.example.com".into())),
        );
        rr.insert(
            "value".to_string(),
            Value::Concrete(ConcreteValue::String("_a1.acm-validations.aws.".into())),
        );
        let mut entry = indexmap::IndexMap::new();
        entry.insert(
            "resource_record".to_string(),
            Value::Concrete(ConcreteValue::Map(rr)),
        );
        let dvo = Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
            ConcreteValue::Map(entry),
        )]));
        let states = states_with_cert(&parsed, "domain_validation_options", dvo);

        // Run 1: no moved targets — the expanded child must be both a
        // new child AND refreshable (regression guard: the #3141 filter
        // must not skip refresh for ordinary expanded children).
        let out_no_moved = expand_same_config_deferred_for(
            &parsed,
            &sorted,
            &crate::managed_current_from_states(states.clone()),
            &DataSourceValues::empty(),
            &carina_core::schema::SchemaRegistry::new(),
            &carina_core::provider::RawSavedAttrs::default()
                .lift(&carina_core::schema::SchemaRegistry::new()),
            &HashMap::new(),
            &HashMap::new(),
            &[] as &[WaitAliasSpec],
            &std::collections::HashSet::new(),
            &std::collections::HashSet::new(),
        )
        .expect("expand without moved targets");

        assert_eq!(
            out_no_moved.new_child_ids.len(),
            1,
            "one RecordSet materialized per domain_validation_options entry"
        );
        let child_id = out_no_moved
            .new_child_ids
            .iter()
            .next()
            .expect("the materialized child id")
            .clone();
        assert!(
            out_no_moved.refreshable_child_ids.contains(&child_id),
            "a non-moved expanded child must still be refreshed"
        );

        // Run 2: declare that same child id as a `moved` `to`.
        let mut moved_targets = std::collections::HashSet::new();
        moved_targets.insert(child_id.clone());
        let out_moved = expand_same_config_deferred_for(
            &parsed,
            &sorted,
            &crate::managed_current_from_states(states.clone()),
            &DataSourceValues::empty(),
            &carina_core::schema::SchemaRegistry::new(),
            &carina_core::provider::RawSavedAttrs::default()
                .lift(&carina_core::schema::SchemaRegistry::new()),
            &HashMap::new(),
            &HashMap::new(),
            &[] as &[WaitAliasSpec],
            &moved_targets,
            &std::collections::HashSet::new(),
        )
        .expect("expand with the child as a moved target");

        assert_eq!(
            out_moved.new_child_ids, out_no_moved.new_child_ids,
            "the moved-exclusion must not change what materialized"
        );
        assert!(
            !out_moved.refreshable_child_ids.contains(&child_id),
            "a child that is a `moved` target must be excluded from \
             refreshable_child_ids so its migrated state is not \
             overwritten by a not_found provider read (carina#3141)"
        );
        assert!(
            out_moved.refreshable_child_ids.is_empty(),
            "the only expanded child is the moved target, so nothing \
             is refreshable; got {:?}",
            out_moved.refreshable_child_ids
        );
    }

    /// Multi-entry variant: ≥2 `domain_validation_options` (real ACM
    /// certs have apex + SAN). Moving only the first index must exclude
    /// exactly that child from refresh and leave the second one
    /// refreshable — the per-index `moved` (`[0]`, `[1]`, …) and the
    /// exclusion set are both exercised, not just the single-entry case.
    #[test]
    fn moved_exclusion_is_per_child_with_multiple_entries() {
        let src = r#"
            let cert = aws.acm.Certificate {
                domain_name       = "r.example.com"
                validation_method = "DNS"
            }

            for (_, opt) in cert.domain_validation_options {
                aws.route53.RecordSet {
                    name             = opt.resource_record.name
                    type             = "CNAME"
                    resource_records = [opt.resource_record.value]
                }
            }
        "#;
        let parsed = parse(src, &ProviderContext::default()).expect("parse");
        let sorted = sort_resources_by_dependencies(&parsed.resources).unwrap();

        let make_entry = |name: &str, value: &str| {
            let mut rr = indexmap::IndexMap::new();
            rr.insert(
                "name".to_string(),
                Value::Concrete(ConcreteValue::String(name.into())),
            );
            rr.insert(
                "value".to_string(),
                Value::Concrete(ConcreteValue::String(value.into())),
            );
            let mut entry = indexmap::IndexMap::new();
            entry.insert(
                "resource_record".to_string(),
                Value::Concrete(ConcreteValue::Map(rr)),
            );
            Value::Concrete(ConcreteValue::Map(entry))
        };
        let dvo = Value::Concrete(ConcreteValue::List(vec![
            make_entry("_apex.r.example.com", "_apex.acm-validations.aws."),
            make_entry("_san.r.example.com", "_san.acm-validations.aws."),
        ]));
        let states = states_with_cert(&parsed, "domain_validation_options", dvo);

        let baseline = expand_same_config_deferred_for(
            &parsed,
            &sorted,
            &crate::managed_current_from_states(states.clone()),
            &DataSourceValues::empty(),
            &carina_core::schema::SchemaRegistry::new(),
            &carina_core::provider::RawSavedAttrs::default()
                .lift(&carina_core::schema::SchemaRegistry::new()),
            &HashMap::new(),
            &HashMap::new(),
            &[] as &[WaitAliasSpec],
            &std::collections::HashSet::new(),
            &std::collections::HashSet::new(),
        )
        .expect("expand");
        assert_eq!(
            baseline.new_child_ids.len(),
            2,
            "two RecordSets materialized (apex + SAN)"
        );

        // Move only one of the two children.
        let mut ids: Vec<ResourceId> = baseline.new_child_ids.iter().cloned().collect();
        ids.sort_by_key(|id| id.to_string());
        let moved_child = ids[0].clone();
        let kept_child = ids[1].clone();

        let mut moved_targets = std::collections::HashSet::new();
        moved_targets.insert(moved_child.clone());
        let out = expand_same_config_deferred_for(
            &parsed,
            &sorted,
            &crate::managed_current_from_states(states.clone()),
            &DataSourceValues::empty(),
            &carina_core::schema::SchemaRegistry::new(),
            &carina_core::provider::RawSavedAttrs::default()
                .lift(&carina_core::schema::SchemaRegistry::new()),
            &HashMap::new(),
            &HashMap::new(),
            &[] as &[WaitAliasSpec],
            &moved_targets,
            &std::collections::HashSet::new(),
        )
        .expect("expand");

        assert_eq!(
            out.new_child_ids, baseline.new_child_ids,
            "moved-exclusion must not change what materialized"
        );
        assert!(
            !out.refreshable_child_ids.contains(&moved_child),
            "the moved child must be excluded from refresh"
        );
        assert!(
            out.refreshable_child_ids.contains(&kept_child),
            "the non-moved child must still be refreshed"
        );
        assert_eq!(
            out.refreshable_child_ids.len(),
            1,
            "exactly one of two children is refreshable; got {:?}",
            out.refreshable_child_ids
        );
    }

    /// carina#3145: a for-loop child that was applied on a previous run
    /// is in the state file. On the next `plan`/`apply`, the phase-1
    /// orphan pass classifies it as an orphan (it is not yet a desired
    /// resource — expansion happens *after* refresh) and performs a live
    /// provider read of it, storing the result in managed current state. The
    /// post-expansion child refresh then reads the *same* address a
    /// second time. Two live provider reads for one resource.
    ///
    /// The fix decides "already live-read this run" *once*, in the same
    /// place the moved-target exclusion is decided, and carries it in the
    /// typed `RefreshableChildIds` so the plan and apply paths cannot
    /// diverge. This test pins that contract: an id in `already_refreshed`
    /// must materialize as a child but be excluded from
    /// `refreshable_child_ids` (no redundant second read), while a child
    /// not in that set stays refreshable.
    #[test]
    fn already_refreshed_child_is_excluded_from_refreshable_set() {
        let src = r#"
            let cert = aws.acm.Certificate {
                domain_name       = "r.example.com"
                validation_method = "DNS"
            }

            for (_, opt) in cert.domain_validation_options {
                aws.route53.RecordSet {
                    name             = opt.resource_record.name
                    type             = "CNAME"
                    resource_records = [opt.resource_record.value]
                }
            }
        "#;
        let parsed = parse(src, &ProviderContext::default()).expect("parse");
        let sorted = sort_resources_by_dependencies(&parsed.resources).unwrap();

        let make_entry = |name: &str, value: &str| {
            let mut rr = indexmap::IndexMap::new();
            rr.insert(
                "name".to_string(),
                Value::Concrete(ConcreteValue::String(name.into())),
            );
            rr.insert(
                "value".to_string(),
                Value::Concrete(ConcreteValue::String(value.into())),
            );
            let mut entry = indexmap::IndexMap::new();
            entry.insert(
                "resource_record".to_string(),
                Value::Concrete(ConcreteValue::Map(rr)),
            );
            Value::Concrete(ConcreteValue::Map(entry))
        };
        let dvo = Value::Concrete(ConcreteValue::List(vec![
            make_entry("_apex.r.example.com", "_apex.acm-validations.aws."),
            make_entry("_san.r.example.com", "_san.acm-validations.aws."),
        ]));
        let states = states_with_cert(&parsed, "domain_validation_options", dvo);

        // Baseline: nothing pre-refreshed — both children materialize and
        // are refreshable (the single live read happens post-expansion).
        let baseline = expand_same_config_deferred_for(
            &parsed,
            &sorted,
            &crate::managed_current_from_states(states.clone()),
            &DataSourceValues::empty(),
            &carina_core::schema::SchemaRegistry::new(),
            &carina_core::provider::RawSavedAttrs::default()
                .lift(&carina_core::schema::SchemaRegistry::new()),
            &HashMap::new(),
            &HashMap::new(),
            &[] as &[WaitAliasSpec],
            &std::collections::HashSet::new(),
            &std::collections::HashSet::new(),
        )
        .expect("expand");
        assert_eq!(
            baseline.new_child_ids.len(),
            2,
            "two RecordSets materialized (apex + SAN)"
        );
        assert_eq!(
            baseline.refreshable_child_ids.len(),
            2,
            "with nothing pre-refreshed both children are refreshable"
        );

        // The phase-1 orphan pass already live-read the first child this
        // run (it is in the state file from a prior apply, not yet a
        // desired resource at orphan time).
        let mut ids: Vec<ResourceId> = baseline.new_child_ids.iter().cloned().collect();
        ids.sort_by_key(|id| id.to_string());
        let already_read = ids[0].clone();
        let still_unread = ids[1].clone();

        let mut already_refreshed = std::collections::HashSet::new();
        already_refreshed.insert(already_read.clone());

        let out = expand_same_config_deferred_for(
            &parsed,
            &sorted,
            &crate::managed_current_from_states(states.clone()),
            &DataSourceValues::empty(),
            &carina_core::schema::SchemaRegistry::new(),
            &carina_core::provider::RawSavedAttrs::default()
                .lift(&carina_core::schema::SchemaRegistry::new()),
            &HashMap::new(),
            &HashMap::new(),
            &[] as &[WaitAliasSpec],
            &std::collections::HashSet::new(),
            &already_refreshed,
        )
        .expect("expand");

        assert_eq!(
            out.new_child_ids, baseline.new_child_ids,
            "the already-refreshed exclusion must not change what materialized"
        );
        assert!(
            !out.refreshable_child_ids.contains(&already_read),
            "a child already live-read by the phase-1 orphan pass must be \
             excluded from refreshable_child_ids so it is not read a \
             second time (carina#3145)"
        );
        assert!(
            out.refreshable_child_ids.contains(&still_unread),
            "a child not yet read this run must still be refreshed"
        );
        assert_eq!(
            out.refreshable_child_ids.len(),
            1,
            "exactly one of two children is refreshable; got {:?}",
            out.refreshable_child_ids
        );
    }
}

/// Tests for the unified "Using <provider>" announcement format
/// (carina#3067). The default-instance and named-instance code paths
/// must emit the same shape so that CI logs are unambiguous about
/// which kind / instance / region was wired up.
#[cfg(test)]
mod using_line_format_tests {
    use super::super::format_provider_using_line;

    #[test]
    fn default_instance_with_source() {
        // Default instance (no binding). Source is always known on this
        // path because the default instance is the only one that
        // load-sources a factory.
        let source = "github.com/carina-rs/carina-provider-awscc"
            .parse()
            .unwrap();
        assert_eq!(
            format_provider_using_line("awscc", "ap-northeast-1", None, Some(&source),),
            "Using awscc (region: ap-northeast-1, instance=default, \
             source: github.com/carina-rs/carina-provider-awscc)",
        );
    }

    #[test]
    fn named_instance_with_source() {
        // Named instance. Source belongs to the kind, not the instance,
        // and is still included so that one log line is enough to
        // identify what got wired up.
        let source = "github.com/carina-rs/carina-provider-aws".parse().unwrap();
        assert_eq!(
            format_provider_using_line("aws", "us-east-1", Some("us"), Some(&source),),
            "Using aws (region: us-east-1, instance=us, \
             source: github.com/carina-rs/carina-provider-aws)",
        );
    }

    #[test]
    fn instance_label_uses_default_marker_when_unbound() {
        // The kind's default instance must be labeled "instance=default"
        // explicitly, not omitted — the omission is what made the old
        // default-line and named-line shapes diverge.
        let line = format_provider_using_line("aws", "ap-northeast-1", None, None);
        assert!(
            line.contains("instance=default"),
            "default instance must surface as 'instance=default'; got: {line}",
        );
    }

    #[test]
    fn kind_label_is_kind_name_not_display_name() {
        // The label must be the kind name ("aws") — what the user
        // writes in `provider <kind> {}` and routes to via
        // `directives.provider` — not the factory's display_name
        // ("AWS provider"). The old named-instance path used
        // display_name, which made the two log lines disagree on
        // what to call the same construct.
        let line = format_provider_using_line("aws", "us-east-1", Some("us"), None);
        assert!(
            line.starts_with("Using aws "),
            "kind label must be the kind name, not a display name; got: {line}",
        );
    }
}

#[cfg(test)]
mod kind_default_source_tests {
    use super::super::kind_default_source;
    use carina_core::parser::{ProviderConfig, ResolvedProviderConfigs};
    use indexmap::IndexMap;

    fn config(name: &str, binding: Option<&str>, source: Option<&str>) -> ProviderConfig {
        ProviderConfig {
            name: name.to_string(),
            attributes: IndexMap::new(),
            default_tags: IndexMap::new(),
            source: source.map(|source| source.parse().unwrap()),
            version: None,
            revision: None,
            unresolved_attributes: IndexMap::new(),
            binding: binding.map(str::to_string),
        }
    }

    fn resolved(configs: Vec<ProviderConfig>) -> ResolvedProviderConfigs {
        let mut parsed = carina_core::parser::InferredFile::default();
        parsed.providers = configs;
        carina_core::parser::resolve_provider_attributes_with_remote(
            &parsed,
            &std::collections::HashMap::new(),
            &carina_core::parser::ProviderContext::default(),
        )
        .unwrap()
    }

    #[test]
    fn returns_source_of_matching_kind_default() {
        let configs = resolved(vec![
            config(
                "aws",
                None,
                Some("github.com/carina-rs/carina-provider-aws"),
            ),
            config("aws", Some("named"), None),
        ]);
        assert_eq!(
            kind_default_source(&configs, "aws").map(ToString::to_string),
            Some("github.com/carina-rs/carina-provider-aws".to_string()),
        );
    }

    #[test]
    fn ignores_named_instances_with_same_kind() {
        // Named instance comes first in the slice. The function must
        // still pick the default, not the named — otherwise it would
        // return the named instance's (always-None) source.
        let configs = resolved(vec![
            config("aws", Some("named"), None),
            config(
                "aws",
                None,
                Some("github.com/carina-rs/carina-provider-aws"),
            ),
        ]);
        assert_eq!(
            kind_default_source(&configs, "aws").map(ToString::to_string),
            Some("github.com/carina-rs/carina-provider-aws".to_string()),
        );
    }

    #[test]
    fn different_kinds_do_not_cross_leak() {
        // Two kinds, each with their own default + source. Asking for
        // one kind must never return the other's source.
        let configs = resolved(vec![
            config(
                "aws",
                None,
                Some("github.com/carina-rs/carina-provider-aws"),
            ),
            config(
                "awscc",
                None,
                Some("github.com/carina-rs/carina-provider-awscc"),
            ),
        ]);
        assert_eq!(
            kind_default_source(&configs, "aws").map(ToString::to_string),
            Some("github.com/carina-rs/carina-provider-aws".to_string()),
        );
        assert_eq!(
            kind_default_source(&configs, "awscc").map(ToString::to_string),
            Some("github.com/carina-rs/carina-provider-awscc".to_string()),
        );
    }

    #[test]
    fn returns_none_when_kind_default_has_no_source() {
        let configs = resolved(vec![config("mock", None, None)]);
        assert_eq!(kind_default_source(&configs, "mock"), None);
    }

    #[test]
    fn returns_none_when_kind_is_absent() {
        let configs = resolved(vec![config(
            "aws",
            None,
            Some("github.com/carina-rs/carina-provider-aws"),
        )]);
        assert_eq!(kind_default_source(&configs, "awscc"), None);
    }
}

/// carina#3688 composition regression: the CLI's one provider-loader seam
/// must combine the host's structured instantiation diagnostic with the
/// resolver's typed lock provenance. Neither half may be re-derived at a
/// command-specific consumer.
#[tokio::test(flavor = "multi_thread")]
async fn installed_revision_instantiation_error_includes_lock_provenance() {
    let user_dirs = carina_core::TestUserDirs::new();
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path();
    let source = "github.com/example/carina-provider-awscc";
    let revision = "main";
    let resolved_sha = test_commit_sha();
    let github_source = match source
        .parse::<carina_core::provider_source::ProviderSource>()
        .unwrap()
    {
        carina_core::provider_source::ProviderSource::Remote(
            carina_core::provider_source::RemoteSource::GithubDirect(source),
        ) => source,
        parsed => panic!("expected GitHub source: {parsed}"),
    };
    let wasm_path = carina_provider_resolver::revision_resolver::cache_path_revision(
        base,
        &github_source,
        &resolved_sha,
    );
    std::fs::create_dir_all(wasm_path.parent().unwrap()).unwrap();
    std::fs::write(
        &wasm_path,
        r#"
            (component
              (type $empty (instance))
              (import "wasi:http/types@0.2.9" (instance (type $empty)))
            )
        "#,
    )
    .unwrap();

    let mut lock = carina_provider_resolver::LockFile::default();
    lock.upsert(carina_provider_resolver::LockEntry {
        name: "awscc".into(),
        source: source.into(),
        kind: carina_provider_resolver::LockEntryKind::Revision {
            revision: revision.into(),
            resolved_sha: resolved_sha.clone(),
            artifact_id: test_artifact_id(42),
        },
        sha256: carina_provider_resolver::sha256_file(&wasm_path).unwrap(),
        registry: None,
    });
    lock.save(&base.join("carina-providers.lock")).unwrap();
    let config = ProviderConfig {
        name: "awscc".into(),
        source: Some(source.parse().unwrap()),
        version: None,
        revision: Some(revision.into()),
        unresolved_attributes: IndexMap::new(),
        binding: None,
        attributes: IndexMap::new(),
        default_tags: IndexMap::new(),
    };
    let cache = tempfile::tempdir().unwrap();
    let cache_path = cache.path().to_path_buf();
    let inputs = TrustedLocalGrantInputs::new(Vec::new(), None);

    let (factories, errors) = build_factories_from_providers_impl_using(
        &user_dirs,
        &[config],
        base,
        false,
        &inputs,
        move |verified| {
            let cache_path = cache_path.clone();
            async move {
                carina_plugin_host::WasmProviderFactory::from_bytes_with_cache_dir(
                    verified,
                    &cache_path,
                )
                .await
            }
        },
    )
    .unwrap();
    assert!(factories.is_empty());
    let rendered = errors
        .get("awscc")
        .expect("factory loading must retain resolver provenance");

    assert!(rendered.starts_with("Failed to instantiate WASM component (HTTP)"));
    assert!(rendered.contains("Provider: awscc"));
    assert!(rendered.contains(&format!("Artifact: {}", wasm_path.display())));
    assert!(rendered.contains("wasi:http/types@0.2.9"));
    assert!(rendered.contains("Host wasi:http version: 0.2.6"));
    assert!(rendered.contains("carina-providers.lock"));
    assert!(rendered.contains("revision main"));
    assert!(rendered.contains(&format!("resolved_sha {TEST_COMMIT_SHA}")));
    assert!(
        rendered.contains(&format!("`carina init --upgrade {}`", base.display())),
        "{rendered}"
    );
}

#[test]
fn missing_locked_revision_artifact_reaches_cli_with_pin_and_init_hint() {
    let user_dirs = carina_core::TestUserDirs::new();
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path();
    let source = "github.com/carina-rs/carina-provider-awscc";
    let revision = "main";
    let resolved_sha = test_commit_sha();
    let lock_path = base.join("carina-providers.lock");
    let mut lock = carina_provider_resolver::LockFile::default();
    lock.upsert(carina_provider_resolver::LockEntry {
        name: "awscc".into(),
        source: source.into(),
        kind: carina_provider_resolver::LockEntryKind::Revision {
            revision: revision.into(),
            resolved_sha,
            artifact_id: test_artifact_id(42),
        },
        sha256: "fixture".into(),
        registry: None,
    });
    lock.save(&lock_path).unwrap();
    let config = ProviderConfig {
        name: "awscc".into(),
        source: Some(source.parse().unwrap()),
        version: None,
        revision: Some(revision.into()),
        unresolved_attributes: IndexMap::new(),
        binding: None,
        attributes: IndexMap::new(),
        default_tags: IndexMap::new(),
    };

    let (factories, errors) = build_factories_from_providers(&user_dirs, &[config], base).unwrap();
    let rendered = errors
        .get("awscc")
        .expect("missing locked artifact must reach the CLI diagnostic boundary");

    assert!(factories.is_empty());
    assert!(rendered.starts_with("Provider 'awscc' not installed."));
    assert!(rendered.contains(&lock_path.display().to_string()));
    assert!(rendered.contains("revision main"));
    assert!(rendered.contains(&format!("resolved_sha {TEST_COMMIT_SHA}")));
    assert!(
        rendered.contains(&format!("Run `carina init {}`", base.display())),
        "{rendered}"
    );
    assert!(!rendered.contains("lock is stale"));
    assert!(!rendered.contains("carina init --upgrade"));
}

#[tokio::test(flavor = "multi_thread")]
async fn registry_source_reaches_cli_artifact_loader() {
    let user_dirs = carina_core::TestUserDirs::new();
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path();
    let source = "example/awscc";
    let version = "0.5.0";
    let provider_source: ProviderSource = source.parse().unwrap();
    let ProviderSource::Remote(remote_source) = &provider_source else {
        panic!("registry source must parse as remote")
    };
    let wasm_path =
        carina_provider_resolver::cache_path_wasm(base, &remote_source.source_key(), version);
    std::fs::create_dir_all(wasm_path.parent().unwrap()).unwrap();
    std::fs::write(&wasm_path, b"not a wasm component").unwrap();

    let mut lock = carina_provider_resolver::LockFile::default();
    lock.upsert(carina_provider_resolver::LockEntry {
        name: "awscc".into(),
        source: source.into(),
        kind: carina_provider_resolver::LockEntryKind::Version {
            version: version.into(),
            constraint: None,
        },
        sha256: carina_provider_resolver::sha256_file(&wasm_path).unwrap(),
        registry: None,
    });
    lock.save(&base.join("carina-providers.lock")).unwrap();

    let config = ProviderConfig {
        name: "awscc".into(),
        source: Some(provider_source),
        version: None,
        revision: None,
        unresolved_attributes: IndexMap::new(),
        binding: None,
        attributes: IndexMap::new(),
        default_tags: IndexMap::new(),
    };

    let (factories, errors) = build_factories_from_providers(&user_dirs, &[config], base).unwrap();
    let rendered = errors
        .get("awscc")
        .expect("the invalid fixture should fail only after registry resolution");

    assert!(factories.is_empty());
    assert!(rendered.contains("carina-providers.lock"), "{rendered}");
    assert!(rendered.contains("version 0.5.0"), "{rendered}");
}

#[tokio::test(flavor = "multi_thread")]
async fn registry_source_reaches_plan_apply_provider_loader() {
    let user_dirs = carina_core::TestUserDirs::new();
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path();
    let source = "example/awscc";
    let version = "0.5.0";
    let provider_source: ProviderSource = source.parse().unwrap();
    let ProviderSource::Remote(remote_source) = &provider_source else {
        panic!("registry source must parse as remote")
    };
    let wasm_path =
        carina_provider_resolver::cache_path_wasm(base, &remote_source.source_key(), version);
    std::fs::create_dir_all(wasm_path.parent().unwrap()).unwrap();
    std::fs::write(&wasm_path, b"not a wasm component").unwrap();

    let mut lock = carina_provider_resolver::LockFile::default();
    lock.upsert(carina_provider_resolver::LockEntry {
        name: "awscc".into(),
        source: source.into(),
        kind: carina_provider_resolver::LockEntryKind::Version {
            version: version.into(),
            constraint: None,
        },
        sha256: carina_provider_resolver::sha256_file(&wasm_path).unwrap(),
        registry: None,
    });
    lock.save(&base.join("carina-providers.lock")).unwrap();

    let config = ProviderConfig {
        name: "awscc".into(),
        source: Some(provider_source),
        version: None,
        revision: None,
        unresolved_attributes: IndexMap::new(),
        binding: None,
        attributes: IndexMap::new(),
        default_tags: IndexMap::new(),
    };
    let (factories, errors) =
        build_factories_from_providers(&user_dirs, std::slice::from_ref(&config), base)
            .expect("registry source must reach the single factory-load boundary");
    let load_error = errors
        .get("awscc")
        .expect("invalid WASM fixture must be reported during factory loading");
    assert!(
        load_error.contains("Failed to load WASM component"),
        "{load_error}"
    );
    assert!(load_error.contains("carina-providers.lock"), "{load_error}");
    assert!(load_error.contains("version 0.5.0"), "{load_error}");

    let configs = validated_provider_configs(vec![config]);
    let ctx = WiringContext::new(factories, configs.as_provider_configs());
    let router = get_provider_with_ctx(&ctx, &configs, ProgressSink::Human)
        .await
        .expect("the router must not retry a factory load that already failed");
    assert!(router.is_empty());
}

#[test]
fn runtime_instantiation_error_retains_installed_artifact_provenance() {
    let tmp = tempfile::tempdir().unwrap();
    let wasm_path = tmp.path().join("local-provider.wasm");
    std::fs::write(&wasm_path, b"fixture").unwrap();
    let config = ProviderConfig {
        name: "local".into(),
        source: Some(format!("file://{}", wasm_path.display()).parse().unwrap()),
        version: None,
        revision: None,
        unresolved_attributes: IndexMap::new(),
        binding: None,
        attributes: IndexMap::new(),
        default_tags: IndexMap::new(),
    };
    let installed =
        carina_provider_resolver::find_file_provider_source(tmp.path(), &config).unwrap();

    let error = runtime_instantiation_error_with_provenance(
        &installed,
        std::io::Error::other("runtime instantiation failed"),
    );
    let cause = error
        .detail()
        .cause()
        .expect("the ProviderError boundary must retain the artifact wrapper");

    assert!(
        cause
            .downcast_ref::<carina_provider_resolver::ProviderArtifactLoadError<std::io::Error>>()
            .is_some()
    );
    let rendered = error.to_string();
    assert!(rendered.contains("runtime instantiation failed"));
    assert!(rendered.contains("Provider: local"));
    assert!(rendered.contains(&format!("Artifact: {}", wasm_path.display())));
    assert!(rendered.contains("provider resolved from file://"));
    assert!(rendered.contains("not controlled by carina-providers.lock"));
}

#[test]
fn formatting_factory_loader_returns_lock_constraint_errors() {
    let user_dirs = carina_core::TestUserDirs::new();
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path();
    let source = "github.com/carina-rs/carina-provider-awscc";
    let mut lock = carina_provider_resolver::LockFile::default();
    lock.upsert(carina_provider_resolver::LockEntry {
        name: "awscc".into(),
        source: source.into(),
        kind: carina_provider_resolver::LockEntryKind::GithubRelease {
            version: "1.0.0".parse().unwrap(),
            tag: serde_json::from_str("\"v1.0.0\"").unwrap(),
            asset_id: serde_json::from_str("42").unwrap(),
            constraint: None,
        },
        sha256: "fixture".into(),
        registry: None,
    });
    lock.save(&base.join("carina-providers.lock")).unwrap();
    let config = ProviderConfig {
        name: "awscc".into(),
        source: Some(source.parse().unwrap()),
        version: Some(carina_core::version_constraint::VersionConstraint::parse("^2.0.0").unwrap()),
        revision: None,
        unresolved_attributes: IndexMap::new(),
        binding: None,
        attributes: IndexMap::new(),
        default_tags: IndexMap::new(),
    };

    let (factories, load_errors) =
        build_factories_from_providers_for_formatting(&user_dirs, &[config], base).unwrap();

    assert!(factories.is_empty());
    assert!(!load_errors.is_empty());
    assert!(
        load_errors
            .values()
            .any(|error| error.to_string().contains("locked at version 1.0.0"))
    );
}

#[test]
fn unversioned_lock_error_reaches_cli_typed_with_restore_remediation() {
    let user_dirs = carina_core::TestUserDirs::new();
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path();
    std::fs::write(
        base.join("carina-providers.lock"),
        r#"[[provider]]
name = "awscc"
source = "github.com/carina-rs/carina-provider-awscc"
mode = "revision"
revision = "main"
resolved_sha = "967e645a7153522ca60ef942183d3fc338fc7c27"
sha256 = "3bd19254ba60717dabdc12c663ef96e0be72e5a2fbc192cf3a5d15ef6578f14f"
"#,
    )
    .unwrap();
    let config = ProviderConfig {
        name: "awscc".into(),
        source: Some(
            "github.com/carina-rs/carina-provider-awscc"
                .parse()
                .unwrap(),
        ),
        version: None,
        revision: Some("main".into()),
        unresolved_attributes: IndexMap::new(),
        binding: None,
        attributes: IndexMap::new(),
        default_tags: IndexMap::new(),
    };

    let runtime_error =
        match build_factories_from_providers(&user_dirs, std::slice::from_ref(&config), base) {
            Err(error) => error,
            Ok(_) => panic!("unversioned provider lock must be returned as an error"),
        };
    let rendered = runtime_error.to_string();
    assert!(
        rendered.contains("predates lock format versioning"),
        "{rendered}"
    );
    assert!(
        rendered.contains("restore this file from version control or backup"),
        "{rendered}"
    );
    assert!(
        rendered.contains("preserve it for manual inspection"),
        "{rendered}"
    );

    assert!(
        matches!(
            &runtime_error,
            AppError::LockConstraint(carina_provider_resolver::LockConstraintError::LockFile(_))
        ),
        "the CLI diagnostic must retain the typed lock-file error"
    );
    assert_eq!(
        rendered.matches("predates lock format versioning").count(),
        1
    );
    assert!(std::error::Error::source(&runtime_error).is_none());

    let (factories, load_errors) =
        build_factories_from_providers_for_formatting(&user_dirs, &[config], base).unwrap();
    assert!(factories.is_empty());
    let formatting_error = load_errors
        .get("carina-providers.lock")
        .expect("formatting diagnostics must name the lock file");
    let formatting_rendered = formatting_error.to_string();
    assert!(
        formatting_rendered.contains("predates lock format versioning"),
        "{formatting_rendered}"
    );
    assert!(
        formatting_rendered.contains("restore this file from version control or backup"),
        "{formatting_rendered}"
    );
    assert!(
        formatting_rendered.contains("preserve it for manual inspection"),
        "{formatting_rendered}"
    );
    assert!(
        matches!(
            formatting_error,
            ProviderFactoryDiagnostic::LockConstraint(
                carina_provider_resolver::LockConstraintError::LockFile(_)
            )
        ),
        "the formatting diagnostic must retain the typed lock-file error"
    );
    assert_eq!(
        formatting_rendered
            .matches("predates lock format versioning")
            .count(),
        1
    );
    assert!(std::error::Error::source(formatting_error).is_none());
}

#[test]
fn runtime_factory_loader_returns_lock_constraint_error_instead_of_exiting() {
    let user_dirs = carina_core::TestUserDirs::new();
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path();
    let source = "github.com/carina-rs/carina-provider-awscc";
    let mut lock = carina_provider_resolver::LockFile::default();
    lock.upsert(carina_provider_resolver::LockEntry {
        name: "awscc".into(),
        source: source.into(),
        kind: carina_provider_resolver::LockEntryKind::GithubRelease {
            version: "1.0.0".parse().unwrap(),
            tag: serde_json::from_str("\"v1.0.0\"").unwrap(),
            asset_id: serde_json::from_str("42").unwrap(),
            constraint: None,
        },
        sha256: "fixture".into(),
        registry: None,
    });
    lock.save(&base.join("carina-providers.lock")).unwrap();
    let config = ProviderConfig {
        name: "awscc".into(),
        source: Some(source.parse().unwrap()),
        version: Some(carina_core::version_constraint::VersionConstraint::parse("^2.0.0").unwrap()),
        revision: None,
        unresolved_attributes: IndexMap::new(),
        binding: None,
        attributes: IndexMap::new(),
        default_tags: IndexMap::new(),
    };

    let result = build_factories_from_providers(&user_dirs, &[config], base);
    let error = match result {
        Err(error) => error,
        Ok(_) => panic!("mismatched provider lock must be returned as an error"),
    };

    let rendered = error.to_string();
    assert!(rendered.contains("locked at version 1.0.0"));
    assert!(
        rendered.contains(&format!("`carina init --upgrade {}`", base.display())),
        "{rendered}"
    );
}

mod resolved_value_constraint_gate {
    use super::super::*;
    use super::{build_override_aware_pair_for_test, canonical_pair, canonical_unresolved};
    use std::sync::atomic::{AtomicUsize, Ordering};

    use carina_core::binding_index::LivePreApplyInputs;
    use carina_core::effect::PlanOp;
    use carina_core::parser::{CompareOp, ValidateExpr};
    use carina_core::plan::PlanErrorKind;
    use carina_core::provider::{
        AppliedIdentifier, BoxFuture, CreateOutcome, CreateRequest, DeleteRequest, ProviderFactory,
        ProviderNormalizer, ProviderResult, ReadOutcome, ReadRequest, UpdateOutcome, UpdateRequest,
        ready_noop,
    };
    use carina_core::resource::{
        AccessPath, DataSource, InterpolationPart, LiveComposition, LiveCompositionSignature,
        ModuleConstraintId, PendingModuleConstraint, UnknownReason,
    };
    use carina_core::schema::{TypeIdentity, ValidatorType};

    fn text(value: &str) -> Value {
        Value::Concrete(ConcreteValue::String(value.to_string().into()))
    }

    struct ConstraintFactory;

    impl ProviderFactory for ConstraintFactory {
        fn name(&self) -> &str {
            "test"
        }

        fn display_name(&self) -> &str {
            "resolved constraint test provider"
        }

        fn provider_config_attribute_types(&self) -> carina_core::schema::ValidatedConfigSchema {
            Default::default()
        }

        fn validate_config(
            &self,
            _attributes: &carina_core::provider::CanonicalProviderConfigAttributes,
        ) -> carina_core::provider::ProviderResult<()> {
            Ok(())
        }

        fn validate_custom_type(
            &self,
            identity: &TypeIdentity,
            value: &carina_core::provider::CanonicalCustomTypeValue,
        ) -> carina_core::provider::ProviderResult<()> {
            if identity.kind == "ExternalId" && value.as_str() == "bad" {
                Err(carina_core::provider::ProviderError::invalid_input(
                    "external id rejected",
                ))
            } else {
                Ok(())
            }
        }

        fn extract_region(&self, _attributes: &IndexMap<String, Value>) -> String {
            "local".to_string()
        }

        fn create_provider(
            &self,
            _binding: Option<&str>,
            _config: &carina_core::provider::ProviderReadyConfig,
        ) -> BoxFuture<'_, ProviderResult<Box<dyn Provider>>> {
            Box::pin(async { Ok(Box::new(MockProvider::new()) as Box<dyn Provider>) })
        }

        fn create_normalizer(
            &self,
            _binding: Option<&str>,
            _config: &carina_core::provider::ProviderReadyConfig,
        ) -> BoxFuture<'_, ProviderResult<Box<dyn ProviderNormalizer>>> {
            Box::pin(async {
                Ok(Box::new(carina_core::provider::NoopNormalizer) as Box<dyn ProviderNormalizer>)
            })
        }

        fn schemas(&self) -> Vec<carina_core::schema::ValidatedResourceSchema> {
            let schema_drafts: Vec<carina_core::schema::ResourceSchema> = {
                let pattern_type =
                    AttributeType::refined_string(None, Some("^good-".to_string()), None, None);
                let custom_type = AttributeType::refined_string(
                    Some(TypeIdentity::bare("ExternalId")),
                    None,
                    None,
                    None,
                );
                vec![
                    ResourceSchema::new("source").attribute(
                        AttributeSchema::optional(
                            "value",
                            AttributeType::union(vec![
                                AttributeType::string(),
                                AttributeType::map(AttributeType::string()),
                            ]),
                        )
                        .build(),
                    ),
                    ResourceSchema::new("consumer.Pattern").attribute(
                        AttributeSchema::optional("target", pattern_type.clone()).build(),
                    ),
                    ResourceSchema::new("consumer.Custom")
                        .attribute(AttributeSchema::optional("target", custom_type).build()),
                    ResourceSchema::new("consumer.Validated")
                        .attribute(
                            AttributeSchema::optional(
                                "tags",
                                AttributeType::map(AttributeType::string()),
                            )
                            .build(),
                        )
                        .with_declared_validators(vec![ValidatorType::TagsKeyValueCheck]),
                    ResourceSchema::new("lookup.Pattern")
                        .attribute(AttributeSchema::optional("target", pattern_type).build())
                        .as_data_source(),
                ]
            };
            schema_drafts
                .into_iter()
                .map(|schema| {
                    schema
                        .validate()
                        .expect("factory test schema must be valid")
                })
                .collect()
        }
    }

    struct RecordingDataSourceFactory {
        reads: Arc<AtomicUsize>,
    }

    struct RecordingDataSourceProvider {
        reads: Arc<AtomicUsize>,
    }

    impl ProviderFactory for RecordingDataSourceFactory {
        fn name(&self) -> &str {
            "test"
        }

        fn display_name(&self) -> &str {
            "module constraint refresh test provider"
        }

        fn provider_config_attribute_types(&self) -> carina_core::schema::ValidatedConfigSchema {
            Default::default()
        }

        fn validate_config(
            &self,
            _attributes: &carina_core::provider::CanonicalProviderConfigAttributes,
        ) -> carina_core::provider::ProviderResult<()> {
            Ok(())
        }

        fn extract_region(&self, _attributes: &IndexMap<String, Value>) -> String {
            "local".to_string()
        }

        fn create_provider(
            &self,
            _binding: Option<&str>,
            _config: &carina_core::provider::ProviderReadyConfig,
        ) -> BoxFuture<'_, ProviderResult<Box<dyn Provider>>> {
            let reads = self.reads.clone();
            Box::pin(async move {
                Ok(Box::new(RecordingDataSourceProvider { reads }) as Box<dyn Provider>)
            })
        }

        fn schemas(&self) -> Vec<carina_core::schema::ValidatedResourceSchema> {
            let schema_drafts: Vec<carina_core::schema::ResourceSchema> =
                vec![ResourceSchema::new("lookup").as_data_source()];
            schema_drafts
                .into_iter()
                .map(|schema| {
                    schema
                        .validate()
                        .expect("factory test schema must be valid")
                })
                .collect()
        }
    }

    impl Provider for RecordingDataSourceProvider {
        fn name(&self) -> &str {
            "recording-data-source"
        }

        fn read(
            &self,
            id: &ResourceId,
            _identifier: &AppliedIdentifier,
            _request: ReadRequest,
        ) -> BoxFuture<'_, ProviderResult<ReadOutcome>> {
            let id = id.clone();
            Box::pin(async move { Ok(ReadOutcome::not_found(id)) })
        }

        fn read_data_source(
            &self,
            resource: &carina_core::provider::ProviderReadyDataSource,
        ) -> BoxFuture<'_, ProviderResult<State>> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            let id = resource.id.clone();
            Box::pin(async move { Ok(State::existing(id, HashMap::new())) })
        }

        fn create(
            &self,
            _id: &ResourceId,
            _request: CreateRequest,
            attempt: carina_core::provider::CreateAttempt,
        ) -> BoxFuture<'_, Result<CreateOutcome, carina_core::provider::CreateFailure>> {
            Box::pin(
                async move { Err(attempt.untracked(ProviderError::internal("unexpected create"))) },
            )
        }

        unexpected_resume_create!();

        fn update(
            &self,
            _id: &ResourceId,
            _identifier: &carina_core::provider::AppliedIdentifier,
            _request: UpdateRequest,
        ) -> BoxFuture<'_, ProviderResult<UpdateOutcome>> {
            Box::pin(async { Err(ProviderError::internal("unexpected update")) })
        }

        fn delete(
            &self,
            _id: &ResourceId,
            _identifier: &carina_core::provider::AppliedIdentifier,
            _request: DeleteRequest,
        ) -> BoxFuture<'_, ProviderResult<()>> {
            Box::pin(async { Err(ProviderError::internal("unexpected delete")) })
        }

        fn required_permissions(
            &self,
            _id: &ResourceId,
            _op: PlanOp,
        ) -> ProviderResult<Vec<String>> {
            Ok(Vec::new())
        }
    }

    #[derive(Default)]
    struct RewritingNormalizer {
        desired_calls: AtomicUsize,
    }

    impl ProviderNormalizer for RewritingNormalizer {
        fn normalize_desired<'a>(
            &'a self,
            resources: &'a mut [carina_core::resource::ProviderResource],
        ) -> BoxFuture<'a, ProviderResult<()>> {
            self.desired_calls.fetch_add(1, Ordering::SeqCst);
            for resource in resources {
                if resource.attributes.contains_key("target") {
                    resource.set_attr("target", text("good-normalized"));
                }
            }
            ready_noop()
        }

        fn normalize_state<'a>(
            &'a self,
            _current_states: &'a mut carina_core::provider::StateTransformMap,
        ) -> BoxFuture<'a, ProviderResult<()>> {
            ready_noop()
        }

        fn hydrate_read_state<'a>(
            &'a self,
            _current_states: &'a mut carina_core::provider::StateTransformMap,
            _saved_attrs: &'a carina_core::provider::SavedAttrs,
        ) -> BoxFuture<'a, ProviderResult<()>> {
            ready_noop()
        }
    }

    fn ref_value() -> Value {
        Value::Deferred(DeferredValue::ResourceRef {
            path: AccessPath::new("producer", "value"),
        })
    }

    fn into_plan_errors(error: PlanPreparationError) -> Vec<carina_core::plan::PlanError> {
        match error {
            PlanPreparationError::Plan(errors) => errors.into_vec(),
            PlanPreparationError::Provider(error) => {
                panic!("test normalizer unexpectedly failed: {error}")
            }
        }
    }

    fn managed_resources(
        consumer_type: &str,
        producer_value: Value,
    ) -> (OverrideAwareResources, Vec<Resource>) {
        let producer = Resource::with_provider("test", "source", "producer", None)
            .with_binding("producer")
            .with_attribute("value", producer_value);
        let resolved_attribute = if consumer_type == "consumer.Validated" {
            "tags"
        } else {
            "target"
        };
        let consumer = Resource::with_provider("test", consumer_type, "consumer", None)
            .with_binding("consumer")
            .with_attribute(resolved_attribute, ref_value());
        let source = vec![producer, consumer];
        let compositions = Vec::<LiveComposition>::new();
        let current_states = HashMap::new();
        let remote_bindings = HashMap::new();
        let wait_aliases = Vec::new();
        let (resources, _) = build_override_aware_pair_for_test(
            canonical_unresolved(source, &SchemaRegistry::new()),
            None::<&carina_state::StateFile>,
            LivePreApplyInputs {
                managed: &[],
                compositions: &compositions,
                managed_current: &current_states,
                data_source_values: &DataSourceValues::empty(),
                remote_bindings: &remote_bindings,
                wait_aliases: &wait_aliases,
            },
        )
        .expect("test references resolve");
        let origins = resources.paired_unresolved_resources();
        (resources, origins)
    }

    async fn prepare_managed(
        consumer_type: &str,
        producer_value: Value,
        normalizer: &RewritingNormalizer,
    ) -> Result<(), Vec<carina_core::plan::PlanError>> {
        let wiring =
            ValidatedWiring::from_factories_for_test(vec![Box::new(ConstraintFactory)], &[]);
        let ctx = wiring.context();
        let (mut resources, origins) = managed_resources(consumer_type, producer_value);
        let compositions = Vec::new();
        let module_gate =
            carina_core::executor::ModuleConstraintGate::from_live_compositions(&compositions);
        let mut current_states = HashMap::new();
        let mut data_source_values = DataSourceValues::empty();
        let mut orphans = Orphans::new();
        let mut data_sources = Vec::new();
        let data_source_origins = Vec::new();
        PlanPreprocessor::new(normalizer, ctx)
            .prepare(
                &mut resources,
                &origins,
                &module_gate,
                &mut current_states,
                &mut data_source_values,
                &mut orphans,
                &[],
                &mut data_sources,
                &data_source_origins,
            )
            .await
            .map_err(into_plan_errors)
    }

    fn pending_composition(
        arguments: Vec<(&str, Value)>,
        constraint: PendingModuleConstraint,
    ) -> LiveComposition {
        LiveComposition {
            id: carina_core::resource::ResolvedResourceId::new(ResourceId::with_identity(
                "_virtual", "checked",
            )),
            signature: LiveCompositionSignature {
                arguments: arguments
                    .into_iter()
                    .map(|(name, value)| {
                        (
                            name.to_string(),
                            carina_core::resource::LiveCompositionArgument::from_value(
                                value,
                                carina_core::parser::TypeExpr::String,
                                carina_core::parser::ArgumentOrigin::Defaulted,
                            ),
                        )
                    })
                    .collect(),
                attributes: IndexMap::new(),
                pending_constraints: vec![constraint],
            },
            binding: Some("checked".to_string()),
            dependency_bindings: BTreeSet::new(),
            module_name: "checked_module".to_string(),
            instance: "root.checked".to_string(),
            call_path: crate::test_composition_call_path(),
            quoted_string_attrs: HashSet::new(),
        }
    }

    fn not_bad(variable: &str) -> ValidateExpr {
        ValidateExpr::Compare {
            lhs: Box::new(ValidateExpr::Var(variable.to_string())),
            op: CompareOp::Ne,
            rhs: Box::new(ValidateExpr::String("bad".to_string())),
        }
    }

    async fn prepare_composition(
        producer_values: Vec<(&str, Value)>,
        composition: &mut LiveComposition,
    ) -> Result<(), Vec<carina_core::plan::PlanError>> {
        let wiring =
            ValidatedWiring::from_factories_for_test(vec![Box::new(ConstraintFactory)], &[]);
        let ctx = wiring.context();
        let mut producer =
            Resource::with_provider("test", "source", "producer", None).with_binding("producer");
        for (name, value) in producer_values {
            producer.set_attr(name, value);
        }
        let compositions = std::slice::from_ref(composition);
        let current_states = HashMap::new();
        let remote_bindings = HashMap::new();
        let wait_aliases = Vec::new();
        let (mut resources, _) = build_override_aware_pair_for_test(
            canonical_unresolved(vec![producer], ctx.schemas()),
            None::<&carina_state::StateFile>,
            LivePreApplyInputs {
                managed: &[],
                compositions,
                managed_current: &current_states,
                data_source_values: &DataSourceValues::empty(),
                remote_bindings: &remote_bindings,
                wait_aliases: &wait_aliases,
            },
        )
        .unwrap();
        let origins = resources.paired_unresolved_resources();
        let mut states = HashMap::new();
        let mut data_source_values = DataSourceValues::empty();
        let mut orphans = Orphans::new();
        let mut data_sources = Vec::new();
        let module_gate = carina_core::executor::ModuleConstraintGate::from_live_compositions(
            std::slice::from_ref(composition),
        );

        PlanPreprocessor::new(&carina_core::provider::NoopNormalizer, ctx)
            .prepare(
                &mut resources,
                &origins,
                &module_gate,
                &mut states,
                &mut data_source_values,
                &mut orphans,
                &[],
                &mut data_sources,
                &[],
            )
            .await
            .map_err(into_plan_errors)
    }

    fn assert_resolved_error(
        errors: &[carina_core::plan::PlanError],
        expected_type: &str,
        expected_attribute: &str,
        expected_message: &str,
    ) {
        assert_eq!(errors.len(), 1, "{errors:#?}");
        assert_eq!(errors[0].resource_id.resource_type, expected_type);
        match &errors[0].kind {
            PlanErrorKind::ResolvedValueConstraint {
                attributes,
                origins,
                message,
            } => {
                assert_eq!(attributes, &[expected_attribute]);
                assert_eq!(origins, &["producer.value"]);
                assert!(message.contains(expected_message), "{message}");
            }
            other => panic!("unexpected plan error: {other:?}"),
        }
        let rendered = errors[0].to_string();
        assert!(rendered.contains(expected_type), "{rendered}");
        assert!(rendered.contains(expected_attribute), "{rendered}");
        assert!(rendered.contains("producer.value"), "{rendered}");
    }

    #[tokio::test]
    async fn resolved_reference_constraint_fails_before_normalization() {
        let normalizer = RewritingNormalizer::default();
        let errors = prepare_managed("consumer.Pattern", text("bad"), &normalizer)
            .await
            .expect_err("resolved invalid pattern must fail planning");

        assert_resolved_error(&errors, "consumer.Pattern", "target", "required pattern");
        assert_eq!(normalizer.desired_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn resolved_secret_reference_constraint_fails_masked_before_normalization() {
        let plaintext = "plan-time-plaintext";
        let normalizer = RewritingNormalizer::default();
        let secret = Value::Deferred(DeferredValue::Secret(Box::new(text(plaintext))));

        let errors = prepare_managed("consumer.Pattern", secret, &normalizer)
            .await
            .expect_err("resolved secret must fail its authored-value constraint");
        let rendered = errors
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");

        assert!(rendered.contains("(secret)"), "errors: {rendered}");
        assert!(
            !rendered.contains(plaintext),
            "secret leaked in plan error: {rendered}"
        );
        assert_eq!(normalizer.desired_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn still_unknown_reference_is_not_rejected() {
        let normalizer = RewritingNormalizer::default();
        let unknown = Value::Deferred(DeferredValue::Unknown(UnknownReason::UpstreamExport {
            path: AccessPath::new("producer", "value"),
        }));

        prepare_managed("consumer.Pattern", unknown, &normalizer)
            .await
            .expect("unknown value must remain pending");
        assert_eq!(normalizer.desired_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn resolved_reference_runs_provider_custom_type_lookup() {
        let normalizer = RewritingNormalizer::default();
        let errors = prepare_managed("consumer.Custom", text("bad"), &normalizer)
            .await
            .expect_err("provider custom type must run at the resolution gate");

        assert_resolved_error(&errors, "consumer.Custom", "target", "external id rejected");
        assert_eq!(normalizer.desired_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn resolved_reference_runs_resource_validator() {
        let normalizer = RewritingNormalizer::default();
        let malformed_tags = Value::Concrete(ConcreteValue::Map(IndexMap::from([
            ("key".to_string(), text("Project")),
            ("value".to_string(), text("carina")),
        ])));
        let errors = prepare_managed("consumer.Validated", malformed_tags, &normalizer)
            .await
            .expect_err("resource validator must run at the resolution gate");

        assert_resolved_error(
            &errors,
            "consumer.Validated",
            "tags",
            "tags map contains both 'key' and 'value'",
        );
        assert_eq!(normalizer.desired_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn resolved_data_source_input_uses_the_same_gate() {
        let ctx = WiringContext::new_for_test(vec![Box::new(ConstraintFactory)], &[]);
        let normalizer = RewritingNormalizer::default();
        let (mut resources, _) = build_override_aware_pair_for_test(
            canonical_unresolved(Vec::new(), ctx.schemas()),
            None::<&carina_state::StateFile>,
            LivePreApplyInputs {
                managed: &[],
                compositions: &[],
                managed_current: &HashMap::new(),
                data_source_values: &DataSourceValues::empty(),
                remote_bindings: &HashMap::new(),
                wait_aliases: &[],
            },
        )
        .unwrap();
        let mut data_sources = vec![
            DataSource::with_provider("test", "lookup.Pattern", "lookup", None)
                .with_attribute("target", text("bad")),
        ];
        let data_source_origins = vec![
            DataSource::with_provider("test", "lookup.Pattern", "lookup", None)
                .with_attribute("target", ref_value()),
        ];
        let module_gate = carina_core::executor::ModuleConstraintGate::from_live_compositions(&[]);
        let mut states = HashMap::new();
        let mut data_source_values = super::test_data_source_values(&data_sources, &HashMap::new());
        let mut orphans = Orphans::new();

        let errors = PlanPreprocessor::new(&normalizer, &ctx)
            .prepare(
                &mut resources,
                &[],
                &module_gate,
                &mut states,
                &mut data_source_values,
                &mut orphans,
                &[],
                &mut data_sources,
                &data_source_origins,
            )
            .await
            .expect_err("invalid resolved data-source input must fail planning");
        let errors = into_plan_errors(errors);

        assert_resolved_error(&errors, "lookup.Pattern", "target", "required pattern");
        assert_eq!(normalizer.desired_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn resolved_module_argument_validation_is_a_structured_plan_error() {
        let source = ref_value();
        let constraint = PendingModuleConstraint {
            id: ModuleConstraintId::argument_validation("value", 0),
            expression: not_bad("value"),
            message: "value must not be bad".to_string(),
        };
        let mut composition = pending_composition(vec![("value", source.clone())], constraint);

        let errors = prepare_composition(vec![("value", text("bad"))], &mut composition)
            .await
            .expect_err("resolved invalid module argument must fail planning");

        assert_eq!(errors.len(), 1, "{errors:#?}");
        match &errors[0].kind {
            PlanErrorKind::ModuleConstraint(diagnostic) => {
                assert_eq!(diagnostic.module, "checked_module");
                assert_eq!(diagnostic.instance, "root.checked");
                assert_eq!(diagnostic.arguments, ["value"]);
                assert_eq!(diagnostic.message, "value must not be bad");
                assert_eq!(
                    diagnostic.actuals,
                    [("value".to_string(), "'bad'".to_string())]
                );
            }
            other => panic!("unexpected plan error: {other:?}"),
        }
        let rendered = errors[0].to_string();
        for expected in [
            "module 'checked_module' (call 'checked')",
            "value",
            "value must not be bad",
            "'bad'",
        ] {
            assert!(rendered.contains(expected), "{rendered}");
        }
        assert_eq!(
            composition.signature.arguments["value"].value(),
            &source,
            "plan evaluation must not replace the stored source expression"
        );
    }

    /// Regression test for carina#3448: plan-time upstream substitutions honor sink refinements.
    #[tokio::test]
    async fn upstream_export_substituted_at_plan_is_revalidated_against_sink_schema() {
        let temp = tempfile::tempdir().unwrap();
        let network_dir = temp.path().join("network");
        let consumer_dir = temp.path().join("consumer");
        std::fs::create_dir_all(&network_dir).unwrap();
        std::fs::create_dir_all(&consumer_dir).unwrap();
        std::fs::write(
            network_dir.join("main.crn"),
            r#"backend local { path = "carina.state.json" }
exports { team_prefix: String = "x" }"#,
        )
        .unwrap();

        let mut upstream_state = StateFile::new();
        upstream_state.insert_export_json_for_test(
            "team_prefix".to_string(),
            serde_json::json!("Tokyo_Platform"),
        );
        std::fs::write(
            network_dir.join("carina.state.json"),
            serde_json::to_string(&upstream_state).unwrap(),
        )
        .unwrap();

        let provider_config = ProviderConfig {
            name: "test".to_string(),
            attributes: IndexMap::new(),
            default_tags: IndexMap::new(),
            source: None,
            version: None,
            revision: None,
            unresolved_attributes: IndexMap::new(),
            binding: None,
        };
        let target = Value::Deferred(DeferredValue::Interpolation(vec![
            InterpolationPart::Expr(Value::Deferred(DeferredValue::ResourceRef {
                path: AccessPath::new("net", "team_prefix"),
            })),
            InterpolationPart::Literal("-logs".into()),
        ]));
        let consumer = Resource::with_provider("test", "consumer.Pattern", "sink", None)
            .with_binding("sink")
            .with_attribute("target", target);
        let unresolved_resources = vec![consumer.clone()];
        let mut parsed = carina_core::parser::InferredFile::default();
        parsed.providers.push(provider_config);
        parsed.resources.push(consumer); // allow: direct — fixture test inspection
        parsed
            .upstream_states
            .push(carina_core::parser::UpstreamState {
                binding: "net".to_string(),
                source: std::path::PathBuf::from("../network"),
            });

        let mut cycle_guard = crate::commands::plan::seed_cycle_guard(&consumer_dir);
        let remote_bindings = crate::commands::plan::load_upstream_states(
            &parsed.upstream_states,
            &consumer_dir,
            &carina_core::parser::ProviderContext::default(),
            &mut cycle_guard,
            crate::commands::plan::UpstreamMissingStatePolicy::Lenient,
        )
        .await
        .expect("upstream state fixture must load");
        let wiring =
            ValidatedWiring::from_factories_for_test(vec![Box::new(ConstraintFactory)], &[]);
        let ctx = wiring.context();
        let provider_configs = carina_core::parser::resolve_provider_attributes_with_remote(
            &parsed,
            &remote_bindings,
            &carina_core::parser::ProviderContext::default(),
        )
        .expect("test provider configs must resolve");
        let canonical_resources = canonical_pair(&parsed, unresolved_resources, ctx.schemas());
        let validated = ValidatedConfig::from_wiring_for_test(wiring, canonical_resources);
        let plan = create_plan_from_parsed_with_upstream_with_ctx(
            validated,
            &provider_configs,
            &None,
            true,
            ProgressSink::Human,
            &remote_bindings,
            &StateBlockClaims::default(),
            &ResolvedStateBlockTargets::default(),
        )
        .await
        .expect("resolved-value violations must be represented by the plan");
        let plan = plan.resources();

        assert_eq!(plan.plan.errors().len(), 1, "{:#?}", plan.plan.errors());
        match &plan.plan.errors()[0].kind {
            PlanErrorKind::ResolvedValueConstraint {
                attributes,
                message,
                origins,
                ..
            } => {
                assert_eq!(attributes, &["target"]);
                assert_eq!(origins, &["net.team_prefix"]);
                assert!(message.contains("Tokyo_Platform-logs"), "{message}");
                assert!(message.contains("required pattern"), "{message}");
            }
            other => panic!("unexpected plan error: {other:?}"),
        }
        assert!(
            plan.plan.effects().is_empty(),
            "constraint failures must not produce effects: {:#?}",
            plan.plan.effects()
        );
    }

    #[tokio::test]
    async fn plan_refresh_reads_data_source_before_reporting_unrelated_module_constraint() {
        let constraint = PendingModuleConstraint {
            id: ModuleConstraintId::argument_validation("value", 0),
            expression: not_bad("value"),
            message: "value must not be bad".to_string(),
        };
        let composition = pending_composition(vec![("value", text("bad"))], constraint);
        let data_source = DataSource::with_provider("test", "lookup", "unrelated", None)
            .with_attribute("query", text("literal"));
        let provider_config = ProviderConfig {
            name: "test".to_string(),
            attributes: IndexMap::new(),
            default_tags: IndexMap::new(),
            source: None,
            version: None,
            revision: None,
            unresolved_attributes: IndexMap::new(),
            binding: None,
        };
        let mut parsed = carina_core::parser::InferredFile::default();
        parsed.providers.push(provider_config);
        parsed.compositions.push(composition);
        parsed.data_sources.push(data_source);
        let reads = Arc::new(AtomicUsize::new(0));
        let wiring = ValidatedWiring::from_factories_for_test(
            vec![Box::new(RecordingDataSourceFactory {
                reads: reads.clone(),
            })],
            &[],
        );
        let ctx = wiring.context();
        let provider_configs = carina_core::parser::resolve_provider_attributes_with_remote(
            &parsed,
            &HashMap::new(),
            &carina_core::parser::ProviderContext::default(),
        )
        .expect("test provider configs must resolve");
        let canonical_resources = canonical_pair(&parsed, Vec::new(), ctx.schemas());
        let validated = ValidatedConfig::from_wiring_for_test(wiring, canonical_resources);
        let plan = create_plan_from_parsed_with_upstream_with_ctx(
            validated,
            &provider_configs,
            &None,
            true,
            ProgressSink::Human,
            &HashMap::new(),
            &StateBlockClaims::default(),
            &ResolvedStateBlockTargets::default(),
        )
        .await
        .expect("module violations must be represented by the plan");
        let plan = plan.resources();

        assert_eq!(
            reads.load(Ordering::SeqCst),
            1,
            "an unrelated module constraint must not block the query phase"
        );
        assert_eq!(plan.plan.errors().len(), 1, "{:#?}", plan.plan.errors());
        assert!(matches!(
            &plan.plan.errors()[0].kind,
            PlanErrorKind::ModuleConstraint(diagnostic)
                if diagnostic.module == "checked_module"
                    && diagnostic.instance == "root.checked"
                    && diagnostic.message == "value must not be bad"
        ));
    }

    #[tokio::test]
    async fn resolved_cross_argument_require_is_a_structured_plan_error() {
        let constraint = PendingModuleConstraint {
            id: ModuleConstraintId::require(0),
            expression: ValidateExpr::Compare {
                lhs: Box::new(ValidateExpr::Var("left".to_string())),
                op: CompareOp::Eq,
                rhs: Box::new(ValidateExpr::Var("right".to_string())),
            },
            message: "left and right must match".to_string(),
        };
        let mut composition = pending_composition(
            vec![
                ("left", ref_value()),
                (
                    "right",
                    Value::Deferred(DeferredValue::ResourceRef {
                        path: AccessPath::new("producer", "other"),
                    }),
                ),
            ],
            constraint,
        );

        let errors = prepare_composition(
            vec![("value", text("left")), ("other", text("right"))],
            &mut composition,
        )
        .await
        .expect_err("resolved invalid require must fail planning");

        match &errors[0].kind {
            PlanErrorKind::ModuleConstraint(diagnostic) => {
                assert_eq!(diagnostic.arguments, ["left", "right"]);
                assert_eq!(diagnostic.message, "left and right must match");
                assert_eq!(
                    diagnostic.actuals,
                    [
                        ("left".to_string(), "'left'".to_string()),
                        ("right".to_string(), "'right'".to_string()),
                    ]
                );
            }
            other => panic!("unexpected plan error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn unknown_module_argument_stays_pending_without_rewriting_source() {
        let source = ref_value();
        let constraint = PendingModuleConstraint {
            id: ModuleConstraintId::argument_validation("value", 0),
            expression: not_bad("value"),
            message: "value must not be bad".to_string(),
        };
        let mut composition = pending_composition(vec![("value", source.clone())], constraint);
        let unknown = Value::Deferred(DeferredValue::Unknown(UnknownReason::UpstreamExport {
            path: AccessPath::new("producer", "value"),
        }));

        prepare_composition(vec![("value", unknown)], &mut composition)
            .await
            .expect("unknown module argument must remain pending");

        assert_eq!(composition.signature.pending_constraints.len(), 1);
        assert_eq!(composition.signature.arguments["value"].value(), &source);
    }

    #[tokio::test]
    async fn unpublished_module_argument_stays_pending_without_plan_error() {
        let source = ref_value();
        let constraint = PendingModuleConstraint {
            id: ModuleConstraintId::argument_validation("value", 0),
            expression: not_bad("value"),
            message: "value must not be bad".to_string(),
        };
        let mut composition = pending_composition(vec![("value", source.clone())], constraint);

        prepare_composition(Vec::new(), &mut composition)
            .await
            .expect("an unpublished argument must remain pending during planning");

        assert_eq!(composition.signature.pending_constraints.len(), 1);
        assert_eq!(composition.signature.arguments["value"].value(), &source);
    }

    #[tokio::test]
    async fn satisfied_module_constraint_is_retained_for_apply_time_recheck() {
        let constraint = PendingModuleConstraint {
            id: ModuleConstraintId::argument_validation("value", 0),
            expression: not_bad("value"),
            message: "value must not be bad".to_string(),
        };
        let mut composition = pending_composition(vec![("value", ref_value())], constraint);

        prepare_composition(vec![("value", text("good"))], &mut composition)
            .await
            .expect("satisfied constraint must pass");

        assert_eq!(composition.signature.pending_constraints.len(), 1);
    }

    #[tokio::test]
    async fn module_constraint_actuals_mask_secret_values() {
        let constraint = PendingModuleConstraint {
            id: ModuleConstraintId::argument_validation("value", 0),
            expression: not_bad("value"),
            message: "value must not be bad".to_string(),
        };
        let mut composition = pending_composition(vec![("value", ref_value())], constraint);
        let secret = Value::Deferred(DeferredValue::Secret(Box::new(text("bad"))));

        let errors = prepare_composition(vec![("value", secret)], &mut composition)
            .await
            .expect_err("invalid secret value must fail without disclosure");
        let rendered = errors[0].to_string();

        assert!(rendered.contains("(secret)"), "{rendered}");
        assert!(!rendered.contains("\"bad\""), "{rendered}");
        assert!(!rendered.contains("Secret("), "{rendered}");
    }
}
