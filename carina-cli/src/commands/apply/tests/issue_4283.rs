//! Physical-record regression: parent generations coexist, while child creates
//! UPSERT the same physical key and old deletes remove that key.
use super::*;
use crate::commands::plan::prepare_plan_identity_and_provider_inputs;
use crate::wiring::{LoadedProviderFactory, RowRequest};

#[derive(Clone, Default)]
struct Cloud(Arc<Mutex<CloudRecords>>);
#[derive(Default)]
struct CloudRecords {
    coexisting_child: bool,
    attribute_child: bool,
    fail_child_delete: bool,
    child_generation: usize,
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

    fn validate_config(
        &self,
        _: &carina_core::provider::CanonicalProviderConfigAttributes,
        _: &carina_core::provider::ProviderValidationInputs,
    ) -> ProviderResult<()> {
        Ok(())
    }
    fn create_provider(
        &self,
        _: &carina_core::provider::ProviderSession,
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
        _: &carina_core::provider::ProviderSession,
        _: carina_core::provider::InitializeOutcomeMint,
    ) -> BoxFuture<'_, ProviderResult<Box<dyn ProviderNormalizer>>> {
        Box::pin(async { Ok(Box::new(NoopNormalizer) as Box<dyn ProviderNormalizer>) })
    }
    fn schema_witness(&self) -> carina_core::schema::ValidatedProviderSchemaWitness {
        carina_core::schema::ValidatedProviderSchemaWitness::new(
            self.schemas(),
            Default::default(),
            Default::default(),
            Vec::new(),
        )
        .unwrap()
    }
    fn schemas(&self) -> Vec<carina_core::schema::ValidatedResourceSchema> {
        let cloud = self.0.0.lock().unwrap();
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
            child_schema(cloud.coexisting_child, cloud.attribute_child),
        ]
    }
}
fn child_schema(coexisting: bool, attribute: bool) -> carina_core::schema::ValidatedResourceSchema {
    let schema = ResourceSchema::new("test.Child")
        .attribute(
            AttributeSchema::required("name", AttributeType::string())
                .create_only()
                .build(),
        )
        .attribute(
            AttributeSchema::required("value", AttributeType::string())
                .create_only()
                .build(),
        );
    if attribute {
        schema.with_unique_name_attribute("name")
    } else if coexisting {
        schema.with_coexisting_replacement()
    } else {
        schema.with_conflicting_replacement()
    }
    .validate()
    .unwrap()
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
            let mut cloud = self.0.0.lock().unwrap();
            let key = if cloud.attribute_child {
                format!("dns:{}", value_string(&attrs["name"]).unwrap())
            } else if cloud.coexisting_child {
                cloud.child_generation += 1;
                format!("dns:{}", cloud.child_generation)
            } else {
                "dns:_validation.example".to_string()
            };
            (key, HashMap::new())
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
        _: carina_core::provider::UpdateAttempt,
    ) -> BoxFuture<'_, ProviderResult<carina_core::provider::UpdateOutcome>> {
        Box::pin(async { panic!("unexpected update") })
    }
    fn delete(
        &self,
        attempt: carina_core::provider::DeleteAttempt,
    ) -> BoxFuture<'_, ProviderResult<()>> {
        let identifier = attempt.identifier();
        let mut cloud = self.0.0.lock().unwrap();
        cloud.operations.push(format!("delete:{identifier}"));
        if cloud.fail_child_delete && identifier.as_str().starts_with("dns:") {
            return Box::pin(async {
                Err(ProviderError::api_error(
                    "injected old-child delete failure",
                ))
            });
        }
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
        crate::commands::plan::SavedBackendConfig::default(),
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
async fn apply_saved(fixture: &ApplyCancellationFixture, cloud: &Cloud, saved_plan: PlanFile) {
    // Exercise actual wire decoding and schema resolution, not a clone of a live plan.
    let json = serde_json::to_string(&saved_plan).unwrap();
    let decoded: PlanFile = serde_json::from_str(&json).unwrap();
    let source = ResolvedPath::new(fixture.config_path()).unwrap();
    let bound = ProviderBoundPlanFile::for_test(
        inspected_saved_plan(decoded),
        providers(fixture, cloud).into_complete().unwrap(),
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
}
async fn convergence(saved: bool) {
    Box::pin(convergence_with_binding(saved, true)).await;
}
async fn convergence_with_binding(saved: bool, bound: bool) {
    let fixture = ApplyCancellationFixture::new().with_raw_config("");
    let cloud = Cloud::default();
    write_config(&fixture, "old");
    if !bound {
        unbind_config(&fixture);
    }
    apply(&fixture, &cloud).await;
    write_config(&fixture, "new");
    if !bound {
        unbind_config(&fixture);
    }
    let saved_plan = plan(&fixture, &cloud).await;
    if saved {
        apply_saved(&fixture, &cloud, saved_plan).await;
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

#[tokio::test]
async fn review2_4283_unbound_runtime_replacement_writeback_live() {
    Box::pin(convergence_with_binding(false, false)).await;
}
#[tokio::test]
async fn review2_4283_unbound_runtime_replacement_writeback_saved() {
    Box::pin(convergence_with_binding(true, false)).await;
}

fn unbind_config(fixture: &ApplyCancellationFixture) {
    let path = fixture.config_path().join("main.crn");
    let source = std::fs::read_to_string(&path)
        .unwrap()
        .replace("let records = for", "for");
    std::fs::write(path, source).unwrap();
}

#[tokio::test]
async fn review2_4283_unbound_create_first_delete_failure_preserves_old() {
    let fixture = ApplyCancellationFixture::new().with_raw_config("");
    let cloud = Cloud::default();
    cloud.0.lock().unwrap().coexisting_child = true;
    write_config(&fixture, "old");
    unbind_config(&fixture);
    apply(&fixture, &cloud).await;
    write_config(&fixture, "new");
    unbind_config(&fixture);
    let path = fixture.config_path().join("main.crn");
    let source = std::fs::read_to_string(&path).unwrap().replace(
        "name = opt.name",
        "directives { create_before_destroy = true }\n    name = opt.name",
    );
    std::fs::write(path, source).unwrap();
    cloud.0.lock().unwrap().fail_child_delete = true;
    let outcome = run_apply_locked(
        config(&fixture, &cloud),
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
    .await;
    assert!(outcome.is_err(), "injected deletion must fail");
    let state = fixture.read_state().await;
    let child = state
        .resources()
        .iter()
        .find(|row| row.resource_type == "test.Child")
        .unwrap();
    assert_eq!(
        child.identifier().map(AppliedIdentifier::as_str),
        Some("dns:2")
    );
    assert_eq!(
        child.deposed.len(),
        1,
        "checkpoint/final writeback lost the still-live old instance: {child:?}"
    );
    assert!(cloud.0.lock().unwrap().live.contains_key("dns:1"));
}

#[tokio::test]
async fn review2_4283_legacy_unbound_runtime_writeback_retires_old_address() {
    let fixture = ApplyCancellationFixture::new().with_raw_config("");
    let cloud = Cloud::default();
    write_config(&fixture, "old");
    unbind_config(&fixture);
    apply(&fixture, &cloud).await;
    let mut state = fixture.read_state().await;
    let mut child = state
        .resources()
        .iter()
        .find(|row| row.resource_type == "test.Child")
        .unwrap()
        .clone();
    state.remove_resource(
        &child.provider,
        &child.resource_type,
        child.identity.as_str(),
    );
    child.identity =
        carina_core::resource::ResourceIdentity::try_from("_options[0]".to_owned()).unwrap();
    child.binding = Some(carina_core::resource::absolute_binding_address_for_test(
        "_options[0]",
    ));
    state.upsert_resource(child).unwrap();
    fixture.write_state(&state);
    write_config(&fixture, "new");
    unbind_config(&fixture);
    apply(&fixture, &cloud).await;
    let state = fixture.read_state().await;
    let children: Vec<_> = state
        .resources()
        .iter()
        .filter(|row| row.resource_type == "test.Child")
        .collect();
    assert_eq!(
        children.len(),
        1,
        "successful replacement must retire its distinct legacy address: {children:?}"
    );
    assert_ne!(children[0].identity.as_str(), "_options[0]");
    assert!(plan(&fixture, &cloud).await.plan.effects().is_empty());
}

#[tokio::test]
async fn review5_4283_saved_plan_conversion_readmits_delete_first_preview() {
    Box::pin(saved_plan_conversion_preview(false, false)).await;
}

#[tokio::test]
async fn review5_4283_saved_plan_conversion_readmits_create_first_preview() {
    Box::pin(saved_plan_conversion_preview(true, false)).await;
}

#[tokio::test]
async fn review5_4283_secret_plan_conversion_readmits_deferred_previews() {
    for create_first in [false, true] {
        Box::pin(saved_plan_conversion_preview(create_first, true)).await;
    }
}

pub(in crate::commands::apply) async fn saved_plan_conversion_fixture(
    create_first: bool,
    secret: bool,
) -> (
    crate::commands::plan::ReloadedPlanFile,
    SchemaRegistry,
    Box<dyn Provider>,
) {
    let fixture = ApplyCancellationFixture::new().with_raw_config("");
    let cloud = Cloud::default();
    cloud.0.lock().unwrap().coexisting_child = create_first;
    write_config(&fixture, "old");
    if secret {
        let path = fixture.config_path().join("main.crn");
        let text = std::fs::read_to_string(&path)
            .unwrap()
            .replace("value = opt.value", "value = secret(opt.value)");
        std::fs::write(path, text).unwrap();
    }
    apply(&fixture, &cloud).await;
    write_config(&fixture, "new");
    let path = fixture.config_path().join("main.crn");
    let mut source_text = std::fs::read_to_string(&path).unwrap();
    if create_first {
        source_text = source_text.replace(
            "name = opt.name",
            "directives { create_before_destroy = true }\n    name = opt.name",
        );
    }
    if secret {
        source_text = source_text.replace("value = opt.value", "value = secret(opt.value)");
    }
    std::fs::write(path, source_text).unwrap();
    let saved = plan(&fixture, &cloud).await;
    let source = ResolvedPath::new(fixture.config_path()).unwrap();
    let mut loaded = config(&fixture, &cloud);
    let mut state = Some(fixture.read_state().await);
    let prepared = prepare_plan_identity_and_provider_inputs(
        &mut loaded,
        &mut state,
        &source,
        fixture.provider_context(),
        UpstreamMissingStatePolicy::Lenient,
    )
    .await
    .unwrap();
    let decoded: PlanFile = serde_json::from_str(&serde_json::to_string(&saved).unwrap()).unwrap();
    let resolved = decoded
        .decode_for_test()
        .reload_in_flight_from_locked_state(state.as_ref())
        .unwrap()
        .resolve_with_schemas(loaded.context().schemas())
        .unwrap();
    let live = if secret {
        rehydrate_desired_plan_file_from_parts(
            resolved,
            loaded.resources().parsed(),
            &prepared.provider_inputs,
            &source,
            ApplyTarget::SavedPlan {
                plan_file: Path::new("p.plan"),
                source_dir: fixture.config_path(),
            },
            loaded.context().provider_context(),
        )
        .unwrap()
    } else {
        resolved.into_live_without_secrets().unwrap()
    };
    (
        live,
        loaded.context().schemas().clone(),
        Box::new(Mock(cloud)),
    )
}

async fn saved_plan_conversion_preview(create_first: bool, secret: bool) {
    let (live, schemas, _) = saved_plan_conversion_fixture(create_first, secret).await;
    let admitted = live.plan().clone().admit(&schemas).unwrap();
    let expected = if create_first { "+/-" } else { "-/+" };
    let glyph = admitted
        .effects()
        .iter()
        .find_map(|effect| match effect {
            carina_core::effect::Effect::DeferredReplace(payload) => Some(payload.display_glyph()),
            _ => None,
        })
        .expect("fixture has a deferred replacement");
    assert_eq!(
        glyph, expected,
        "approval must show the admitted replacement order"
    );
}

#[tokio::test]
async fn review4_4283_attribute_child_with_deposed_name_live() {
    Box::pin(attribute_child_with_deposed_name(false)).await;
}

#[tokio::test]
async fn review4_4283_attribute_child_with_deposed_name_saved() {
    Box::pin(attribute_child_with_deposed_name(true)).await;
}

async fn attribute_child_with_deposed_name(saved: bool) {
    let fixture = ApplyCancellationFixture::new().with_raw_config("");
    let cloud = Cloud::default();
    cloud.0.lock().unwrap().attribute_child = true;
    write_config(&fixture, "old");
    apply(&fixture, &cloud).await;
    let mut state = fixture.read_state().await;
    let old = state
        .resources()
        .iter()
        .find(|row| row.resource_type == "test.Child")
        .unwrap()
        .clone();
    let id = ResourceId::with_provider_identity("awscc", "test.Child", old.identity.clone(), None);
    let mut attributes = cloud.0.lock().unwrap().live["dns:_validation.example"].clone();
    attributes.insert("name".into(), string_value("current-B"));
    cloud
        .0
        .lock()
        .unwrap()
        .live
        .insert("dns:current-B".into(), attributes.clone());
    let mut child = carina_state::ResourceState::from_provider_state(
        "test.Child",
        old.identity.clone(),
        "awscc",
        &State::<NoPlaceholder>::existing(id, attributes).with_identifier("dns:current-B"),
    )
    .unwrap();
    child.binding = old.binding.clone();
    child.explicit = old.explicit.clone();
    child.dependency_bindings = old.dependency_bindings.clone();
    child.directives = old.directives.clone();
    child
        .deposed
        .push(carina_state::DeposedInstance::from_persisted_json(
            carina_state::DeposedKey::new_unique(),
            "dns:_validation.example".into(),
            None,
            old.persisted_attributes().unwrap().to_decoded_json(),
            old.dependency_bindings.clone(),
        ));
    state.upsert_resource(child).unwrap();
    fixture.write_state(&state);
    write_config(&fixture, "new");
    let path = fixture.config_path().join("main.crn");
    let source = std::fs::read_to_string(&path).unwrap().replace(
        "name = opt.name",
        "directives { create_before_destroy = true }\n    name = opt.name",
    );
    std::fs::write(path, source).unwrap();
    cloud.0.lock().unwrap().operations.clear();
    if saved {
        let saved_plan = plan(&fixture, &cloud).await;
        apply_saved(&fixture, &cloud, saved_plan).await;
    } else {
        apply(&fixture, &cloud).await;
    }
    let state = fixture.read_state().await;
    let child = state
        .resources()
        .iter()
        .find(|row| row.resource_type == "test.Child")
        .unwrap();
    let identifier = child.identifier().unwrap().as_str();
    assert_ne!(
        identifier, "dns:_validation.example",
        "the deposed generation still owns A during Create"
    );
    assert_ne!(identifier, "dns:current-B");
    assert!(child.deposed.is_empty());
    assert_eq!(
        child.name_overrides["name"].original_value.as_deref(),
        Some("_validation.example")
    );
    {
        let cloud = cloud.0.lock().unwrap();
        assert!(
            cloud.live.contains_key(identifier),
            "old deletion removed the new physical record: {:?}",
            cloud.operations
        );
        let position = |op: &str| {
            cloud
                .operations
                .iter()
                .position(|actual| actual == op)
                .unwrap()
        };
        let create = position(&format!("create:{identifier}"));
        assert!(position("create:parent-new") < create);
        assert!(
            create < position("delete:dns:_validation.example"),
            "operations: {:?}",
            cloud.operations
        );
        assert!(create < position("delete:dns:current-B"));
        assert!(position("delete:dns:_validation.example") < position("delete:parent-old"));
    }
    assert!(plan(&fixture, &cloud).await.plan.effects().is_empty());
    destroy(&fixture, &cloud).await;
    assert!(cloud.0.lock().unwrap().live.is_empty());
    assert!(fixture.read_state().await.resources().is_empty());
}
