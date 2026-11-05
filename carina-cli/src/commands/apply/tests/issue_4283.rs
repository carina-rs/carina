//! Physical-record regression: parent generations coexist, while child creates
//! UPSERT the same physical key and old deletes remove that key.
use super::*;
use crate::wiring::{LoadedProviderFactory, RowRequest};

#[derive(Clone, Default)]
struct Cloud(Arc<Mutex<CloudRecords>>);
#[derive(Default)]
struct CloudRecords {
    live: HashMap<String, HashMap<String, Value>>,
    operations: Vec<String>,
}
struct Factory(Cloud);
struct Mock(Cloud);
impl ProviderFactory for Factory {
    fn name(&self) -> &str {
        "awscc"
    }
    fn display_name(&self) -> &str {
        "replacement order fixture"
    }
    fn provider_config_attribute_types(&self) -> carina_core::schema::ValidatedConfigSchema {
        Default::default()
    }
    fn validate_config(
        &self,
        _: &carina_core::provider::CanonicalProviderConfigAttributes,
    ) -> ProviderResult<()> {
        Ok(())
    }
    fn create_provider(
        &self,
        _: Option<&str>,
        _: &carina_core::provider::ProviderReadyConfig,
        _: carina_core::provider::InitializeOutcomeMint,
    ) -> BoxFuture<'_, ProviderResult<carina_core::provider::ProviderInitialization>> {
        let cloud = self.0.clone();
        Box::pin(async move {
            Ok(carina_core::provider::InitializeOutcome::unavailable()
                .with_provider(Box::new(Mock(cloud))))
        })
    }
    fn create_normalizer(
        &self,
        _: Option<&str>,
        _: &carina_core::provider::ProviderReadyConfig,
    ) -> BoxFuture<'_, ProviderResult<Box<dyn ProviderNormalizer>>> {
        Box::pin(async { Ok(Box::new(NoopNormalizer) as Box<dyn ProviderNormalizer>) })
    }
    fn schema_witness(&self) -> carina_core::schema::ValidatedProviderSchemaWitness {
        carina_core::schema::ValidatedProviderSchemaWitness::new(
            self.schemas(),
            self.provider_config_attribute_types(),
            Default::default(),
            Vec::new(),
        )
        .unwrap()
    }
    fn schemas(&self) -> Vec<carina_core::schema::ValidatedResourceSchema> {
        vec![
            ResourceSchema::new("test.Parent")
                .attribute(
                    AttributeSchema::required("generation", AttributeType::string())
                        .create_only()
                        .build(),
                )
                .attribute(
                    AttributeSchema::provider_populated(
                        "options",
                        AttributeType::list(AttributeType::struct_(
                            "Option",
                            vec![
                                carina_core::schema::StructField::required(
                                    "name",
                                    AttributeType::string(),
                                )
                                .build(),
                                carina_core::schema::StructField::required(
                                    "value",
                                    AttributeType::string(),
                                )
                                .build(),
                            ],
                        )),
                    )
                    .build(),
                )
                .with_coexisting_replacement()
                .validate()
                .unwrap(),
            ResourceSchema::new("test.Child")
                .attribute(
                    AttributeSchema::required("name", AttributeType::string())
                        .create_only()
                        .build(),
                )
                .attribute(
                    AttributeSchema::required("value", AttributeType::string())
                        .create_only()
                        .build(),
                )
                .with_conflicting_replacement()
                .validate()
                .unwrap(),
        ]
    }
}
impl Provider for Mock {
    fn name(&self) -> &str {
        "awscc"
    }
    fn read(
        &self,
        id: &ResourceId,
        identifier: &AppliedIdentifier,
        _: ReadRequest,
    ) -> BoxFuture<'_, ProviderResult<ReadOutcome>> {
        let state = self
            .0
            .0
            .lock()
            .unwrap()
            .live
            .get(identifier.as_str())
            .cloned()
            .map(|attrs| {
                State::<NoPlaceholder>::existing(id.clone(), attrs)
                    .with_identifier(identifier.as_str())
            });
        let id = id.clone();
        Box::pin(async move {
            Ok(state
                .map(fixture_read_outcome)
                .unwrap_or_else(|| ReadOutcome::not_found(id)))
        })
    }
    fn read_data_source(
        &self,
        _: &carina_core::provider::ProviderReadyDataSource,
    ) -> BoxFuture<'_, ProviderResult<State>> {
        Box::pin(async { panic!("unexpected data source") })
    }
    fn create(
        &self,
        id: &ResourceId,
        request: CreateRequest,
        attempt: carina_core::provider::CreateAttempt,
    ) -> BoxFuture<
        '_,
        Result<carina_core::provider::CreateOutcome, carina_core::provider::CreateFailure>,
    > {
        let mut attrs = request.resource().as_resource().resolved_attributes();
        let (key, outputs) = if id.resource_type == "test.Parent" {
            let generation = value_string(&attrs["generation"]).unwrap();
            let options = Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                ConcreteValue::Map(IndexMap::from([
                    ("name".into(), string_value("_validation.example")),
                    ("value".into(), string_value("_stable.validation")),
                ])),
            )]));
            (
                format!("parent-{generation}"),
                HashMap::from([("options".into(), options)]),
            )
        } else {
            // Equivalent child generations overwrite one record, just like Route53 UPSERT.
            ("dns:_validation.example".to_string(), HashMap::new())
        };
        attrs.extend(outputs.clone());
        let mut cloud = self.0.0.lock().unwrap();
        cloud.operations.push(format!("create:{key}"));
        cloud.live.insert(key.clone(), attrs);
        Box::pin(async move {
            Ok(attempt
                .issued(
                    AppliedIdentifier::new(key).unwrap(),
                    carina_core::provider::Sent::AllRequestAttributes,
                )
                .succeeded(outputs))
        })
    }
    unexpected_resume_create!();
    fn update(
        &self,
        _: &ResourceId,
        _: &AppliedIdentifier,
        _: UpdateRequest,
    ) -> BoxFuture<'_, ProviderResult<carina_core::provider::UpdateOutcome>> {
        Box::pin(async { panic!("unexpected update") })
    }
    fn delete(
        &self,
        _: &ResourceId,
        identifier: &AppliedIdentifier,
        _: DeleteRequest,
    ) -> BoxFuture<'_, ProviderResult<()>> {
        let mut cloud = self.0.0.lock().unwrap();
        cloud.operations.push(format!("delete:{identifier}"));
        cloud.live.remove(identifier.as_str());
        Box::pin(async { Ok(()) })
    }
    fn required_permissions(
        &self,
        _: &ResourceId,
        _: carina_core::effect::PlanOp,
    ) -> ProviderResult<Vec<String>> {
        Ok(Vec::new())
    }
}
fn write_config(fixture: &ApplyCancellationFixture, generation: &str) {
    std::fs::write(
        fixture.config_path().join("providers.crn"),
        "provider awscc { source = \"file:///test-providers/awscc.wasm\" }\n",
    )
    .unwrap();
    std::fs::write(
        fixture.config_path().join("main.crn"),
        format!(
            r#"
let parent = awscc.test.Parent {{ generation = "{generation}" }}
let records = for opt in parent.options {{
  awscc.test.Child {{
    name = opt.name
    value = opt.value
  }}
}}
"#
        ),
    )
    .unwrap();
}
fn providers(fixture: &ApplyCancellationFixture, cloud: &Cloud) -> ProviderLoad {
    let loaded = load_configuration_with_config(
        &ResolvedPath::new(fixture.config_path()).unwrap(),
        fixture.provider_context(),
        &SchemaRegistry::new(),
    )
    .unwrap();
    ProviderLoad::for_test_with_provider_phase(
        fixture.runtime(),
        LoadedProviderFactory::for_test(vec![Box::new(Factory(cloud.clone()))]),
        &loaded.parsed.providers,
    )
}
fn config(
    fixture: &ApplyCancellationFixture,
    cloud: &Cloud,
) -> ValidatedConfig<
    crate::wiring::CanonicalResourcePair<carina_core::parser::ExpandedInferredExportParam>,
> {
    let project = ResolvedPath::new(fixture.config_path()).unwrap();
    let loaded = load_configuration_with_config(
        &project,
        fixture.provider_context(),
        &SchemaRegistry::new(),
    )
    .unwrap();
    providers(fixture, cloud)
        .validate_config_pair(
            loaded.parsed,
            loaded.unresolved_parsed,
            &project,
            false,
            &loaded.duplicate_declarations,
        )
        .unwrap()
}
async fn apply(fixture: &ApplyCancellationFixture, cloud: &Cloud) {
    run_apply_locked(
        config(fixture, cloud),
        true,
        fixture.backend_handle(),
        StateLock::disabled_for_test(),
        fixture.config_path(),
        fixture.provider_context(),
        fixture.cancel_token(),
        &fixture.observer_factory(),
        NonZeroUsize::new(2).unwrap(),
        false,
    )
    .await
    .unwrap();
}
async fn plan(fixture: &ApplyCancellationFixture, cloud: &Cloud) -> PlanFile {
    let source = ResolvedPath::new(fixture.config_path()).unwrap();
    let mut validated = config(fixture, cloud);
    let mut state = Some(fixture.read_state().await);
    let prepared = prepare_plan_identity_and_provider_inputs(
        &mut validated,
        &mut state,
        &source,
        fixture.provider_context(),
        UpstreamMissingStatePolicy::Lenient,
    )
    .await
    .unwrap();
    let planned = crate::wiring::create_plan_from_parsed_with_upstream_with_ctx(
        validated,
        prepared.provider_inputs.configs(),
        &state,
        true,
        ProgressSink::MachineStdout,
        prepared.provider_inputs.remote_bindings(),
        &prepared.state_blocks.claims,
        &prepared.state_blocks.targets,
        RowRequest::None,
    )
    .await
    .unwrap();
    assert!(!planned.resources().plan.has_errors());
    crate::commands::plan::build_plan_file_from_parts(
        &source,
        planned.resources().parsed(),
        prepared.provider_inputs.configs(),
        None,
        &state,
        planned.resources(),
        &planned,
    )
    .unwrap()
}
async fn destroy(fixture: &ApplyCancellationFixture, cloud: &Cloud) {
    let source = ResolvedPath::new(fixture.config_path()).unwrap();
    let loaded =
        load_configuration_with_config(&source, fixture.provider_context(), &SchemaRegistry::new())
            .unwrap();
    let validated = providers(fixture, cloud)
        .validate_evaluated_config(loaded.parsed, &source, true, &loaded.duplicate_declarations)
        .unwrap();
    crate::commands::destroy::run_destroy_locked_with_ctx(
        validated,
        true,
        fixture.backend_handle(),
        None,
        StateLock::disabled_for_test(),
        true,
        false,
        &source,
        fixture.provider_context(),
        NonZeroUsize::new(2).unwrap(),
        fixture.cancel_token(),
    )
    .await
    .unwrap();
}
async fn convergence(saved: bool) {
    let fixture = ApplyCancellationFixture::new().with_raw_config("");
    let cloud = Cloud::default();
    write_config(&fixture, "old");
    apply(&fixture, &cloud).await;
    write_config(&fixture, "new");
    let saved_plan = plan(&fixture, &cloud).await;
    if saved {
        // Exercise actual wire decoding and schema resolution, not a clone of a live plan.
        let json = serde_json::to_string(&saved_plan).unwrap();
        let decoded: PlanFile = serde_json::from_str(&json).unwrap();
        let source = ResolvedPath::new(fixture.config_path()).unwrap();
        let bound = ProviderBoundPlanFile::for_test(
            inspected_saved_plan(decoded),
            providers(&fixture, &cloud).into_complete().unwrap(),
            &source,
        )
        .unwrap();
        let backend = fixture.backend_handle();
        let outcome = run_apply_from_plan_locked_owned(
            fixture.runtime().color_mode(),
            bound,
            fixture.provider_context(),
            true,
            backend.clone(),
            StateLock::disabled_for_test(),
            ApplyTarget::SavedPlan {
                plan_file: Path::new("p.plan"),
                source_dir: fixture.config_path(),
            },
            &source,
            fixture.cancel_token(),
            &fixture.observer_factory(),
            NonZeroUsize::new(2).unwrap(),
            false,
        )
        .await;
        finish_lock_command(
            fixture.runtime().color_mode(),
            backend.as_ref(),
            outcome,
            &fixture.cancel_token(),
        )
        .await
        .unwrap();
    } else {
        apply(&fixture, &cloud).await;
    }
    let state = fixture.read_state().await;
    let children: Vec<_> = state
        .resources()
        .iter()
        .filter(|r| r.resource_type == "test.Child")
        .collect();
    assert_eq!(children.len(), 1);
    assert!(children[0].deposed.is_empty());
    {
        let cloud = cloud.0.lock().unwrap();
        assert!(
            cloud.live.contains_key("dns:_validation.example"),
            "successful apply must leave the physical child alive; operations: {:?}",
            cloud.operations
        );
        let record = &cloud.live["dns:_validation.example"];
        assert_eq!(
            value_string(&record["name"]).as_deref(),
            Some("_validation.example")
        );
        assert_eq!(
            value_string(&record["value"]).as_deref(),
            Some("_stable.validation")
        );
        assert!(cloud.live.contains_key("parent-new"));
        assert!(!cloud.live.contains_key("parent-old"));
        let position = |op: &str| {
            cloud
                .operations
                .iter()
                .rposition(|candidate| candidate == op)
                .unwrap()
        };
        assert!(position("create:parent-new") < position("delete:dns:_validation.example"));
        assert!(
            position("delete:dns:_validation.example") < position("create:dns:_validation.example")
        );
        assert!(position("create:dns:_validation.example") < position("delete:parent-old"));
    }
    let replanned = plan(&fixture, &cloud).await;
    assert!(
        replanned.plan.effects().is_empty(),
        "replan: {:?}",
        replanned.plan.effects()
    );
    destroy(&fixture, &cloud).await;
    assert!(cloud.0.lock().unwrap().live.is_empty());
    assert!(fixture.read_state().await.resources().is_empty());
}
#[tokio::test]
async fn issue_4283_live_apply_preserves_conflicting_child() {
    convergence(false).await;
}
#[tokio::test]
async fn issue_4283_saved_plan_apply_preserves_conflicting_child() {
    convergence(true).await;
}
