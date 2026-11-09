//! Phase 4 regressions. These deliberately use real admission and scheduling.
use super::*;
use crate::input_rules::FinalizedPlan;
use crate::schema::{InputPath, InputPredicate, InputRule, InputRuleCode, InputRuleScope};

fn schemas(scope: InputRuleScope) -> SchemaRegistry {
    schemas_with_order(scope, true)
}

fn schemas_with_order(scope: InputRuleScope, coexisting: bool) -> SchemaRegistry {
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "",
        (if coexisting {
            PatternResourceSchema::new("test").with_coexisting_replacement()
        } else {
            PatternResourceSchema::new("test").with_conflicting_replacement()
        })
        .attribute(PatternAttributeSchema::optional("name", PatternAttributeType::string()).build())
        .attribute(
            PatternAttributeSchema::optional("value", PatternAttributeType::string()).build(),
        )
        .input_rule(InputRule::value(
            scope,
            InputPath::new("name").unwrap(),
            InputRuleCode::new("test.name.not_ipv4").unwrap(),
            InputPredicate::NotIpv4,
        ))
        .validate()
        .unwrap(),
    );
    schemas
}

fn string(value: &str) -> Value {
    Value::Concrete(ConcreteValue::String(value.into()))
}

async fn assert_late_secret_map_failure_is_protected(update: bool) {
    const PRIVATE_KEY: &str = "private-late-rule-key";
    let mut schemas = SchemaRegistry::new();
    let local = PatternAttributeType::struct_with_rules(
        "Object",
        vec![
            PatternStructField::optional("guard", PatternAttributeType::bool()).build(),
            PatternStructField::optional("target", PatternAttributeType::string()).build(),
        ],
        vec![],
        vec![
            InputRule::value(
                InputRuleScope::EveryWrite,
                InputPath::new("target").unwrap(),
                InputRuleCode::new("test.late.target").unwrap(),
                InputPredicate::Present,
            )
            .with_condition(crate::schema::InputCondition::Equals {
                path: InputPath::new("guard").unwrap(),
                literal: crate::schema::InputLiteral::Bool(true),
            }),
        ],
    );
    schemas.insert(
        "",
        PatternResourceSchema::new("test")
            .attribute(
                PatternAttributeSchema::optional("value", PatternAttributeType::bool()).build(),
            )
            .attribute(
                PatternAttributeSchema::optional("objects", PatternAttributeType::map(local))
                    .build(),
            )
            .validate()
            .unwrap(),
    );
    let objects = Value::Concrete(ConcreteValue::Map(indexmap::IndexMap::from([(
        PRIVATE_KEY.into(),
        Value::Concrete(ConcreteValue::Map(indexmap::IndexMap::from([(
            "guard".into(),
            Value::root_resource_ref("source", "value", vec![]),
        )]))),
    )])))
    .into_secret();
    let mut consumer = Resource::new("test", "consumer").with_attribute("objects", objects);
    consumer.binding = Some(absolute_binding("consumer"));
    consumer
        .dependencies
        .value_bindings
        .insert(absolute_binding("source"));
    let mut resources = resolved_fixture_resources(vec![source(), consumer]);
    let consumer = resources.remove(1);
    let source = resources.remove(0);
    let provider = MockProvider::new();
    provider.push_create(Ok(State::existing(
        source.id.clone(),
        crate::test_support::snapshot_attributes(HashMap::from([(
            "value".into(),
            Value::Concrete(ConcreteValue::Bool(true)),
        )])),
    )
    .with_identifier("source-id")));
    let mut plan = Plan::new();
    plan.add(Effect::Create(source));
    if update {
        provider.push_update(Ok(ok_state(&consumer.id)));
        plan.add(Effect::Update {
            from: Box::new(
                State::existing(
                    consumer.id.clone(),
                    crate::test_support::snapshot_attributes(HashMap::from([(
                        "objects".into(),
                        Value::Concrete(ConcreteValue::Map(indexmap::IndexMap::from([(
                            PRIVATE_KEY.into(),
                            Value::Concrete(ConcreteValue::Map(indexmap::IndexMap::from([(
                                "guard".into(),
                                Value::Concrete(ConcreteValue::Bool(false)),
                            )]))),
                        )]))),
                    )])),
                )
                .with_identifier("old-id"),
            ),
            identifier: AppliedIdentifier::new("old-id").unwrap(),
            to: consumer,
            changed_attributes: vec!["objects".into()],
        });
    } else {
        provider.push_create(Ok(ok_state(&consumer.id)));
        plan.add(Effect::Create(consumer));
    }
    let checked = FinalizedPlan::new(plan.clone())
        .check_inputs(&schemas, &HashMap::new())
        .unwrap();
    assert_eq!(checked.pending_inputs().len(), 1);
    let (result, _) = run(
        &provider,
        plan,
        &schemas,
        HashMap::new(),
        Default::default(),
        &NoopNormalizer,
    )
    .await;
    assert_eq!((result.success_count, result.failure_count()), (1, 1));
    assert_eq!(provider.captured_create_resources().len(), 1);
    assert!(provider.captured_update_requests().is_empty());
    let failure = result.failures()[0].kind();
    assert_eq!(failure.label(), "input rule violation");
    assert!(!format!("{:?}", result.failures()[0]).contains(PRIVATE_KEY));
    let mut error: Option<&dyn std::error::Error> = Some(failure);
    while let Some(current) = error {
        let rendered = format!("{current}\n{current:?}");
        assert!(
            !rendered.contains(PRIVATE_KEY),
            "secret map key leaked at dispatch: {rendered}"
        );
        error = current.source();
    }
}

#[tokio::test]
async fn input_discharge_create_protects_secret_map_keys_in_late_rule_failures() {
    assert_late_secret_map_failure_is_protected(false).await;
}

#[tokio::test]
async fn input_discharge_update_protects_secret_map_keys_in_late_rule_failures() {
    assert_late_secret_map_failure_is_protected(true).await;
}

#[tokio::test]
async fn input_discharge_enum_identifier_is_proven_on_its_wire_string() {
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "",
        PatternResourceSchema::new("test")
            .attribute(
                PatternAttributeSchema::optional(
                    "name",
                    PatternAttributeType::enum_(
                        crate::schema::enum_identity("Name", None),
                        Some(vec!["valid-name".into(), "192.168.1.1".into()]),
                        vec![
                            ("valid-name".into(), "valid".into()),
                            ("192.168.1.1".into(), "invalid".into()),
                        ],
                        None,
                        None,
                    ),
                )
                .build(),
            )
            .input_rule(InputRule::value(
                InputRuleScope::CreateOnly,
                InputPath::new("name").unwrap(),
                InputRuleCode::new("test.name.not_ipv4").unwrap(),
                InputPredicate::NotIpv4,
            ))
            .validate()
            .unwrap(),
    );
    for (identifier, valid) in [("valid", true), ("invalid", false)] {
        let resource =
            resolved_fixture_resources(vec![Resource::new("test", "enum").with_attribute(
                "name",
                Value::Concrete(ConcreteValue::enum_identifier(identifier)),
            )])
            .remove(0);
        let mut plan = Plan::new();
        plan.add(Effect::Create(resource.clone()));
        let admission = FinalizedPlan::new(plan.clone()).check_inputs(&schemas, &HashMap::new());
        if valid {
            assert!(admission.unwrap().pending_inputs().is_empty());
            let provider = MockProvider::new();
            provider.push_create(Ok(ok_state(&resource.id)));
            let (result, events) = run(
                &provider,
                plan,
                &schemas,
                HashMap::new(),
                Default::default(),
                &NoopNormalizer,
            )
            .await;
            assert_eq!(
                (result.success_count, result.failure_count()),
                (1, 0),
                "{events:?}"
            );
            assert_eq!(
                crate::protocol_convert::try_core_to_protocol_value(
                    &(provider.captured_create_resources()[0].attributes["name"])
                        .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
                        .map(|proof| proof.into_classification())
                        .unwrap()
                        .into_known()
                        .unwrap()
                )
                .unwrap(),
                carina_provider_protocol::Value::String("valid-name".into())
            );
        } else {
            assert!(matches!(
                admission.unwrap_err().errors()[0].kind,
                crate::plan::PlanErrorKind::InputRule { .. }
            ));
        }
    }
}

fn source() -> Resource {
    let mut resource = Resource::new("test", "source");
    resource.binding = Some(absolute_binding("source"));
    resource
}

fn consumer() -> Resource {
    let mut resource = Resource::new("test", "consumer");
    resource.binding = Some(absolute_binding("consumer"));
    resource.set_attr("name", Value::root_resource_ref("source", "value", vec![]));
    resource
        .dependencies
        .value_bindings
        .insert(absolute_binding("source"));
    resource
}

fn replacement(plan: &mut Plan, create: ResolvedResource, cbd: bool) {
    replacement_with_delete_dependencies(plan, create, cbd, Default::default());
}

fn replacement_with_delete_dependencies(
    plan: &mut Plan,
    create: ResolvedResource,
    cbd: bool,
    dependencies: crate::resource::PersistedDestroyDependencies,
) {
    let old = DeferredReplaceDelete {
        id: create.resolved_id(),
        identifier: AppliedIdentifier::new("old-id").unwrap(),
        generation: EffectGeneration::Current,
        previous_attributes: HashMap::new(),
        directives: Default::default(),
        binding: create.binding.clone(),
        dependencies,
        explicit_dependencies: Default::default(),
        blocked_by_updates: Default::default(),
    };
    plan.add_replacement(crate::plan::ReplacementGroup {
        replacement: crate::test_support::checked_replacement(create, old, cbd),
        changed_create_only: crate::effect::ChangedCreateOnly::new(vec!["generation".into()])
            .unwrap(),
        cascade_ref_hints: vec![],
        consumer_updates: Default::default(),
    });
}

struct ChangingNormalizer {
    calls: AtomicUsize,
    first: &'static str,
}

async fn round2_initial_create_after_normalization(
    retain_runtime_rule: bool,
) -> Result<Option<crate::replacement::CreatePermit>, ProviderPreparationError> {
    let admission_schemas = schemas(InputRuleScope::CreateOnly);
    let resource = Resource::new("test", "consumer").with_attribute("name", string("valid-name"));
    let mut parsed = crate::parser::ExpandedInferredFile::default();
    parsed.resources.push(resource.clone()); // allow: direct — fixture test inspection
    let graph = crate::resolved_graph::resolve_expanded_graph(parsed.into()).unwrap();
    let checked = graph
        .initial_create_plan(resource, &admission_schemas)
        .unwrap()
        .check_inputs_with_types(
            &admission_schemas,
            &crate::input_rules::InputTypeContext::empty(),
        )
        .unwrap();
    let mut runtime_schemas = admission_schemas;
    if !retain_runtime_rule {
        // Discharge must retain the admitted declarations, even when the
        // provider-preparation catalog has no input rules for this resource.
        runtime_schemas.insert(
            "",
            PatternResourceSchema::new("test")
                .attribute(
                    PatternAttributeSchema::optional("name", PatternAttributeType::string())
                        .build(),
                )
                .validate()
                .unwrap(),
        );
    }
    let bindings = ResolvedBindings::default();
    let gate = ModuleConstraintGate::from_live_compositions(&[]);
    let normalizer = ChangingNormalizer {
        calls: AtomicUsize::new(0),
        first: "192.168.1.1",
    };
    crate::validation::deferred_types::with_session_for_test_async(
        &crate::parser::LoadedProviderContext::empty_for_test(),
        async |session| {
            let host_normalizer = crate::provider::ConfiguredNormalizer::fixture(&normalizer);
            let context = ProviderPreparationContext::new(
                &bindings,
                &gate,
                &[],
                &host_normalizer,
                &[],
                &runtime_schemas,
                session,
            );
            prepare_initial_create(&checked, &context).await
        },
    )
    .await
}

#[tokio::test]
async fn round2_initial_create_discharges_the_original_plan_obligations() {
    let result = round2_initial_create_after_normalization(false).await;
    assert!(
        matches!(result, Err(ProviderPreparationError::InputDischarge(EffectFailureKind::InputRule { ref violation, .. })) if violation.code().as_str() == "test.name.not_ipv4"),
        "the original admitted rule must reject the normalized request: {result:?}"
    );
}

#[tokio::test]
async fn round2_initial_create_discharge_exposes_its_error_source() {
    let error = round2_initial_create_after_normalization(true)
        .await
        .unwrap_err();
    let source = std::error::Error::source(&error)
        .expect("provider preparation must expose its input failure source");
    assert!(matches!(
        source.downcast_ref::<EffectFailureKind>(),
        Some(EffectFailureKind::InputRule { .. })
    ));
    assert!(
        source.to_string().contains("test.name.not_ipv4"),
        "{source}"
    );
}

#[tokio::test]
async fn round2_initial_create_discharge_renders_diagnostics_on_the_generic_error_path() {
    let error = round2_initial_create_after_normalization(true)
        .await
        .unwrap_err();
    let mut rendered = error.to_string();
    let mut source = std::error::Error::source(&error);
    while let Some(error) = source {
        rendered.push_str(&format!("\n{error}"));
        source = error.source();
    }
    assert!(rendered.contains("test.name.not_ipv4"), "{rendered}");
    assert!(rendered.contains("name"), "{rendered}");
    assert!(!rendered.contains("192.168.1.1"), "{rendered}");
}

impl crate::provider::ProviderNormalizer for ChangingNormalizer {
    fn normalize_desired<'a>(
        &'a self,
        resources: &'a mut [crate::resource::ProviderResource],
    ) -> BoxFuture<'a, ProviderResult<()>> {
        Box::pin(async move {
            for resource in resources
                .iter_mut()
                .filter(|resource| resource.id == consumer().id)
            {
                let ordinal = self.calls.fetch_add(1, Ordering::SeqCst);
                resource.set_attr(
                    "name",
                    string(if ordinal == 0 {
                        self.first
                    } else {
                        "192.168.123.234"
                    }),
                );
            }
            Ok(())
        })
    }
    fn normalize_state<'a>(
        &'a self,
        _: &'a mut crate::provider::StateTransformMap,
    ) -> BoxFuture<'a, ProviderResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn hydrate_read_state<'a>(
        &'a self,
        _: &'a mut crate::provider::StateTransformMap,
        _: &'a crate::provider::SavedAttrs,
    ) -> BoxFuture<'a, ProviderResult<()>> {
        Box::pin(async { Ok(()) })
    }
}

#[tokio::test]
async fn input_discharge_delete_first_retains_normalized_request_across_delete() {
    let schemas = schemas(InputRuleScope::CreateOnly);
    let mut resources = resolved_fixture_resources(vec![source(), consumer()]);
    let provider = MockProvider::new();
    provider.push_create(Ok(State::existing(
        resources[0].id.clone(),
        crate::test_support::snapshot_attributes(HashMap::from([(
            "value".into(),
            string("before-normalization"),
        )])),
    )
    .with_identifier("source-id")));
    provider.push_create(Ok(ok_state(&resources[1].id)));
    provider.push_delete(Ok(()));
    let mut plan = Plan::new();
    plan.add(Effect::Create(resources.remove(0)));
    replacement(&mut plan, resources.remove(0), false);
    let normalizer = ChangingNormalizer {
        calls: AtomicUsize::new(0),
        first: "exact-prepared-name",
    };
    let (result, events) = run(
        &provider,
        plan,
        &schemas,
        HashMap::new(),
        Default::default(),
        &normalizer,
    )
    .await;
    assert_eq!(
        (result.success_count, result.failure_count()),
        (3, 0),
        "{events:?}"
    );
    assert_eq!(normalizer.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        provider.captured_create_resources()[1].attributes["name"],
        string("exact-prepared-name")
    );
    assert_eq!(
        provider
            .calls()
            .iter()
            .map(|(op, _)| op.as_str())
            .collect::<Vec<_>>(),
        ["create", "delete", "create"]
    );
}

#[tokio::test]
async fn input_discharge_normalization_invalidates_static_proof_before_delete() {
    let schemas = schemas(InputRuleScope::CreateOnly);
    let mut resource = consumer();
    resource.set_attr("name", string("statically-valid"));
    resource.dependencies.value_bindings.clear();
    let resource = resolved_fixture_resources(vec![resource]).remove(0);
    let old = Current::from_snapshot(ok_state(&resource.id).with_identifier("old-id"));
    let mut plan = Plan::new();
    replacement(&mut plan, resource, false);
    let provider = MockProvider::new();
    let normalizer = ChangingNormalizer {
        calls: AtomicUsize::new(0),
        first: "192.168.123.234",
    };
    let (result, events) = run(
        &provider,
        plan,
        &schemas,
        HashMap::from([(consumer().id, old.clone())]),
        Default::default(),
        &normalizer,
    )
    .await;
    assert_eq!(result.failure_count(), 1, "{events:?}");
    assert_eq!(result.failures()[0].kind().label(), "input rule violation");
    assert!(provider.calls().is_empty());
    assert_eq!(result.managed_current.get(&consumer().id), Some(&old));
}

#[tokio::test]
async fn input_discharge_cascade_preparation_excludes_teardown_and_dispatch_order() {
    let schemas = schemas(InputRuleScope::CreateOnly);
    let mut parent = consumer();
    parent.id = ResourceId::with_identity("test", "parent");
    parent.binding = Some(absolute_binding("parent"));
    let mut child = consumer();
    child.id = ResourceId::with_identity("test", "child");
    child.binding = Some(absolute_binding("child"));
    child.dependencies.depends_on = crate::parser::UnexpandedDependencies::new_for_test(vec![
        crate::parser::UnexpandedDependencyOperand::BareBinding(absolute_binding("parent")),
    ]);
    let resources = resolved_fixture_resources(vec![source(), parent, child]);
    let mut plan = Plan::new();
    plan.add(Effect::Create(resources[0].clone()));
    replacement(&mut plan, resources[1].clone(), false);
    replacement_with_delete_dependencies(
        &mut plan,
        resources[2].clone(),
        false,
        crate::resource::PersistedDestroyDependencies::from_test_dependencies([
            resources[1].resolved_id()
        ]),
    );
    let provider = MockProvider::new();
    provider.push_create(Ok(State::existing(
        resources[0].id.clone(),
        crate::test_support::snapshot_attributes(HashMap::from([(
            "value".into(),
            string("valid-name"),
        )])),
    )
    .with_identifier("source-id")));
    provider.push_create(Ok(ok_state(&resources[1].id)));
    provider.push_create(Ok(ok_state(&resources[2].id)));
    provider.push_delete(Ok(()));
    provider.push_delete(Ok(()));
    let (result, events) = run(
        &provider,
        plan,
        &schemas,
        HashMap::new(),
        Default::default(),
        &NoopNormalizer,
    )
    .await;
    assert_eq!(
        (result.success_count, result.failure_count()),
        (5, 0),
        "{events:?}"
    );
    let calls = provider.calls();
    assert!(
        call_position(&calls, "delete", "test.child")
            < call_position(&calls, "delete", "test.parent")
    );
    assert!(
        call_position(&calls, "create", "test.parent")
            < call_position(&calls, "create", "test.child")
    );
}

async fn run(
    provider: &MockProvider,
    plan: Plan,
    schemas: &SchemaRegistry,
    current: HashMap<ResourceId, Current>,
    bindings: ResolvedBindings,
    normalizer: &dyn crate::provider::ProviderNormalizer,
) -> (ExecutionResult, Vec<String>) {
    run_with_configs(provider, plan, schemas, current, bindings, normalizer, &[]).await
}

async fn run_with_configs(
    provider: &MockProvider,
    plan: Plan,
    schemas: &SchemaRegistry,
    current: HashMap<ResourceId, Current>,
    bindings: ResolvedBindings,
    normalizer: &dyn crate::provider::ProviderNormalizer,
    configs: &[crate::parser::ExpandedProviderConfig],
) -> (ExecutionResult, Vec<String>) {
    let checked = FinalizedPlan::new(plan)
        .check_inputs(schemas, &current)
        .unwrap();
    let mut origins = HashMap::from([(
        ResolvedResourceId::new(source().id.clone()),
        UnresolvedResource::from_pre_resolve(source()),
    )]);
    for effect in checked.effects() {
        let resource = match effect {
            Effect::Create(resource) | Effect::ResumeCreate { resource, .. } => Some(resource),
            Effect::Update { to, .. } => Some(to),
            _ => None,
        };
        if let Some(resource) = resource {
            origins.insert(
                resource.resolved_id(),
                UnresolvedResource::from_pre_resolve(resource.as_inner().clone()),
            );
        }
    }
    let observer = MockObserver::new();
    let outcome = execute_plan(
        provider,
        ExecutionInput {
            plan: &checked,
            provider_context: crate::parser::ProviderContext::clone(
                &crate::parser::LoadedProviderContext::empty_for_test(),
            ),
            provider_check_inputs: ProviderCheckInputs::PlanNormalized {
                resource_origins: &origins,
                data_source_origins: &[],
            },
            compositions: &[],
            bindings,
            managed_current: current,
            data_source_values: ExecutableDataSourceValues::empty(),
            normalizer: &crate::provider::ConfiguredNormalizer::fixture(normalizer),
            provider_configs: configs,
            factories: &[],
            schemas,
            parallelism: NonZeroUsize::new(8).unwrap(),
            precompleted_success_count: 0,
            checkpoint_sink: ExecutionCheckpointSink::noop(),
        },
        &observer,
        uncancelled_shutdown(),
    )
    .await;
    (completed_result(outcome), observer.events())
}

#[tokio::test]
async fn input_discharge_unknown_create_sends_exact_prepared_request_once() {
    let schemas = schemas(InputRuleScope::CreateOnly);
    let resources = resolved_fixture_resources(vec![source(), consumer()]);
    let provider = MockProvider::new();
    provider.push_create(Ok(State::existing(
        resources[0].id.clone(),
        crate::test_support::snapshot_attributes(HashMap::from([(
            "value".into(),
            string("valid-prepared-name"),
        )])),
    )
    .with_identifier("source-id")));
    provider.push_create(Ok(ok_state(&resources[1].id)));
    let mut plan = Plan::new();
    for resource in resources {
        plan.add(Effect::Create(resource));
    }
    let (result, events) = run(
        &provider,
        plan,
        &schemas,
        HashMap::new(),
        Default::default(),
        &NoopNormalizer,
    )
    .await;
    assert_eq!(
        (result.success_count, result.failure_count()),
        (2, 0),
        "{events:?}"
    );
    let captured = provider.captured_create_resources();
    let writes: Vec<_> = captured.iter().filter(|r| r.id == consumer().id).collect();
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0].attributes["name"], string("valid-prepared-name"));
}

#[tokio::test]
async fn input_discharge_consumes_admitted_rules_instead_of_replacing_them_from_the_registry() {
    let admitted_schemas = schemas(InputRuleScope::CreateOnly);
    let resources = resolved_fixture_resources(vec![source(), consumer()]);
    let mut plan = Plan::new();
    plan.add(Effect::Create(resources[1].clone()));
    let checked = FinalizedPlan::new(plan)
        .check_inputs(&admitted_schemas, &HashMap::new())
        .unwrap();
    assert_eq!(checked.pending_inputs().len(), 1);
    let mut runtime_schemas = SchemaRegistry::new();
    runtime_schemas.insert(
        "",
        PatternResourceSchema::new("test")
            .attribute(
                PatternAttributeSchema::optional("name", PatternAttributeType::string()).build(),
            )
            .validate()
            .unwrap(),
    );
    let origins = HashMap::from([
        (
            resources[0].resolved_id(),
            UnresolvedResource::from_pre_resolve(source()),
        ),
        (
            resources[1].resolved_id(),
            UnresolvedResource::from_pre_resolve(consumer()),
        ),
    ]);
    let mut bindings = ResolvedBindings::default();
    bindings.set_with_completeness(
        "source",
        HashMap::from([("value".into(), string("192.168.123.234"))]),
        crate::binding_index::BindingValueSource::Local,
        crate::binding_index::BindingCompleteness::Complete,
    );
    let provider = MockProvider::new();
    provider.push_create(Ok(ok_state(&resources[1].id)));
    let result = completed_result(
        execute_plan(
            &provider,
            ExecutionInput {
                plan: &checked,
                provider_context: crate::parser::ProviderContext::clone(
                    &crate::parser::LoadedProviderContext::empty_for_test(),
                ),
                provider_check_inputs: ProviderCheckInputs::PlanNormalized {
                    resource_origins: &origins,
                    data_source_origins: &[],
                },
                compositions: &[],
                bindings,
                managed_current: HashMap::new(),
                data_source_values: ExecutableDataSourceValues::empty(),
                normalizer: &NoopNormalizer,
                provider_configs: &[],
                factories: &[],
                schemas: &runtime_schemas,
                parallelism: NonZeroUsize::new(8).unwrap(),
                precompleted_success_count: 0,
                checkpoint_sink: ExecutionCheckpointSink::noop(),
            },
            &MockObserver::new(),
            uncancelled_shutdown(),
        )
        .await,
    );
    assert_eq!(result.failure_count(), 1);
    assert!(matches!(
        result.failures()[0].kind(),
        EffectFailureKind::InputRule { .. }
    ));
    assert!(provider.calls().is_empty());
}

#[tokio::test]
async fn input_discharge_violation_does_not_pair_an_unrelated_delete_by_resource_id() {
    let schemas = schemas(InputRuleScope::CreateOnly);
    let mut resources = resolved_fixture_resources(vec![source(), consumer()]);
    let provider = MockProvider::new();
    provider.push_create(Ok(State::existing(
        source().id,
        crate::test_support::snapshot_attributes(HashMap::from([(
            "value".into(),
            string("192.168.123.234"),
        )])),
    )
    .with_identifier("source-id")));
    provider.push_delete(Ok(()));
    let mut plan = Plan::new();
    plan.add(Effect::Create(resources.remove(0)));
    // The same display identity in a different resource type is an unrelated
    // row. Only admitted slot/lineage associations may gate its deletion.
    let mut unrelated_id = consumer().id;
    unrelated_id.resource_type = "Unrelated".into();
    plan.add(Effect::Delete {
        id: ResolvedResourceId::new(unrelated_id.clone()),
        identifier: AppliedIdentifier::new("old-id").unwrap(),
        generation: EffectGeneration::Current,
        directives: Default::default(),
        binding: None,
        dependencies: Default::default(),
        explicit_dependencies: Default::default(),
        blocked_by_updates: Default::default(),
    });
    plan.add(Effect::Create(resources.remove(0)));
    let checked = FinalizedPlan::new(plan.clone())
        .check_inputs(&schemas, &HashMap::new())
        .unwrap();
    assert_eq!(checked.pending_inputs().len(), 1);
    assert_eq!(checked.pending_inputs()[0].target().effect_index(), 2);
    assert!(checked.pending_inputs()[0].paired_deletes().is_empty());
    let (result, events) = run(
        &provider,
        plan,
        &schemas,
        HashMap::new(),
        Default::default(),
        &NoopNormalizer,
    )
    .await;
    assert_eq!(result.success_count, 2, "{events:?}");
    assert_eq!(result.failure_count(), 1, "{events:?}");
    let EffectFailureKind::InputRule { violation } = result.failures()[0].kind() else {
        panic!("expected a typed input rule violation: {events:?}");
    };
    assert_eq!(violation.code().as_str(), "test.name.not_ipv4");
    assert_eq!(violation.path().as_str(), "name");
    assert_eq!(provider.captured_create_resources().len(), 1);
    assert_eq!(provider.captured_create_resources()[0].id, source().id);
    assert!(
        provider
            .calls()
            .contains(&("delete".into(), unrelated_id.to_string()))
    );
    assert_eq!(result.successfully_deleted.len(), 1);
}

#[tokio::test]
async fn input_discharge_violation_is_typed_value_free_and_preserves_both_replacement_orders() {
    for order in [None, Some(false), Some(true)] {
        let schemas = schemas(InputRuleScope::CreateOnly);
        let mut resources = resolved_fixture_resources(vec![source(), consumer()]);
        let provider = MockProvider::new();
        provider.push_create(Ok(State::existing(
            resources[0].id.clone(),
            crate::test_support::snapshot_attributes(HashMap::from([(
                "value".into(),
                string("192.168.123.234"),
            )])),
        )
        .with_identifier("source-id")));
        let old = Current::from_snapshot(ok_state(&resources[1].id).with_identifier("old-id"));
        let mut plan = Plan::new();
        plan.add(Effect::Create(resources.remove(0)));
        if let Some(cbd) = order {
            replacement(&mut plan, resources.remove(0), cbd);
        } else {
            plan.add(Effect::Create(resources.remove(0)));
        }
        let current = if order.is_some() {
            HashMap::from([(consumer().id, old.clone())])
        } else {
            HashMap::new()
        };
        let (result, events) = run(
            &provider,
            plan,
            &schemas,
            current,
            Default::default(),
            &NoopNormalizer,
        )
        .await;
        assert_eq!(
            result.success_count, 1,
            "only the prerequisite may run: {events:?}"
        );
        assert_eq!(result.failure_count(), 1, "{events:?}");
        assert_eq!(result.failures()[0].kind().label(), "input rule violation");
        let diagnostic = format!(
            "{:?} {} {events:?}",
            result.failures(),
            result.failures()[0]
        );
        assert!(diagnostic.contains("test.name.not_ipv4"), "{diagnostic}");
        assert!(!diagnostic.contains("192.168.123.234"), "{diagnostic}");
        assert_eq!(
            provider.calls(),
            vec![("create".into(), source().id.to_string())]
        );
        if order.is_some() {
            assert_eq!(result.managed_current.get(&consumer().id), Some(&old));
            assert!(result.successfully_deleted.is_empty());
        }
    }
}

#[tokio::test]
async fn input_discharge_every_write_update_uses_runtime_value() {
    for value in ["valid-update", "192.168.123.234"] {
        let schemas = schemas(InputRuleScope::EveryWrite);
        let mut resources = resolved_fixture_resources(vec![source(), consumer()]);
        let target = resources.remove(1);
        let provider = MockProvider::new();
        provider.push_create(Ok(State::existing(
            resources[0].id.clone(),
            crate::test_support::snapshot_attributes(HashMap::from([(
                "value".into(),
                string(value),
            )])),
        )
        .with_identifier("source-id")));
        provider.push_update(Ok(ok_state(&target.id)));
        let mut plan = Plan::new();
        plan.add(Effect::Create(resources.remove(0)));
        plan.add(Effect::Update {
            from: Box::new(ok_state(&target.id)),
            identifier: AppliedIdentifier::new("old-id").unwrap(),
            to: target,
            changed_attributes: vec!["name".into()],
        });
        let admission = FinalizedPlan::new(plan.clone())
            .check_inputs(&schemas, &HashMap::new())
            .unwrap();
        assert_eq!(admission.pending_inputs().len(), 1);
        let (result, events) = run(
            &provider,
            plan,
            &schemas,
            HashMap::new(),
            Default::default(),
            &NoopNormalizer,
        )
        .await;
        assert_eq!(
            result.success_count,
            if value == "valid-update" { 2 } else { 1 },
            "{events:?}"
        );
        let requests = provider.captured_update_requests();
        if value == "valid-update" {
            assert_eq!(requests.len(), 1);
            assert_eq!(
                requests[0].patch().ops[0].sent_value(),
                Some(&string(value))
            );
        } else {
            assert!(requests.is_empty());
            assert_eq!(result.failures()[0].kind().label(), "input rule violation");
        }
    }
}

#[tokio::test]
async fn input_discharge_refreshes_resolved_guard_outside_the_update_patch() {
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "",
        PatternResourceSchema::new("test")
            .attribute(
                PatternAttributeSchema::optional("guard", PatternAttributeType::bool()).build(),
            )
            .attribute(
                PatternAttributeSchema::optional("target", PatternAttributeType::string()).build(),
            )
            .attribute(
                PatternAttributeSchema::optional("value", PatternAttributeType::bool()).build(),
            )
            .input_rule(
                InputRule::value(
                    InputRuleScope::EveryWrite,
                    InputPath::new("target").unwrap(),
                    InputRuleCode::new("test.target.suffix").unwrap(),
                    InputPredicate::NoSuffix(crate::schema::NonEmptyString::new(".fifo").unwrap()),
                )
                .with_condition(crate::schema::InputCondition::Equals {
                    path: InputPath::new("guard").unwrap(),
                    literal: crate::schema::InputLiteral::Bool(true),
                }),
            )
            .validate()
            .unwrap(),
    );
    let mut target = Resource::new("test", "consumer")
        .with_attribute("guard", Value::root_resource_ref("source", "value", vec![]))
        .with_attribute("target", string("valid-update"));
    target.binding = Some(absolute_binding("consumer"));
    target
        .dependencies
        .value_bindings
        .insert(absolute_binding("source"));
    let mut resources = resolved_fixture_resources(vec![source(), target]);
    let target = resources.remove(1);
    let provider = MockProvider::new();
    provider.push_create(Ok(State::existing(
        resources[0].id.clone(),
        crate::test_support::snapshot_attributes(HashMap::from([(
            "value".into(),
            Value::Concrete(ConcreteValue::Bool(true)),
        )])),
    )
    .with_identifier("source-id")));
    provider.push_update(Ok(ok_state(&target.id)));
    let previous = State::existing(
        target.id.clone(),
        crate::test_support::snapshot_attributes(HashMap::from([
            ("guard".into(), Value::Concrete(ConcreteValue::Bool(true))),
            ("target".into(), string("previous-name")),
        ])),
    )
    .with_identifier("old-id");
    let mut plan = Plan::new();
    plan.add(Effect::Create(resources.remove(0)));
    plan.add(Effect::Update {
        from: Box::new(previous),
        identifier: AppliedIdentifier::new("old-id").unwrap(),
        to: target,
        changed_attributes: vec!["target".into()],
    });
    let admission = FinalizedPlan::new(plan.clone())
        .check_inputs(&schemas, &HashMap::new())
        .unwrap();
    assert_eq!(admission.pending_inputs().len(), 1);
    let (result, events) = run(
        &provider,
        plan,
        &schemas,
        HashMap::new(),
        Default::default(),
        &NoopNormalizer,
    )
    .await;
    assert_eq!(
        (result.success_count, result.failure_count()),
        (2, 0),
        "{events:?}"
    );
    let requests = provider.captured_update_requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].patch().ops.len(), 1);
    assert_eq!(requests[0].patch().ops[0].key(), "target");
    assert_eq!(
        requests[0].patch().ops[0].sent_value(),
        Some(&string("valid-update"))
    );
}

#[tokio::test]
async fn input_discharge_keeps_ignored_unsent_target_unresolved() {
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "",
        PatternResourceSchema::new("test")
            .attribute(
                PatternAttributeSchema::optional("guard", PatternAttributeType::bool()).build(),
            )
            .attribute(
                PatternAttributeSchema::optional("target", PatternAttributeType::string()).build(),
            )
            .input_rule(
                InputRule::value(
                    InputRuleScope::EveryWrite,
                    InputPath::new("target").unwrap(),
                    InputRuleCode::new("test.target.required").unwrap(),
                    InputPredicate::Equals(crate::schema::InputLiteral::String("required".into())),
                )
                .with_condition(crate::schema::InputCondition::Equals {
                    path: InputPath::new("guard").unwrap(),
                    literal: crate::schema::InputLiteral::Bool(true),
                }),
            )
            .validate()
            .unwrap(),
    );
    let mut desired = Resource::new("test", "consumer")
        .with_attribute("guard", Value::Concrete(ConcreteValue::Bool(true)))
        .with_attribute("target", string("required"));
    desired.directives.ignore_changes = vec![crate::resource::ParsedIgnorePath::new(vec![
        crate::resource::ParsedIgnorePathSegment::Attribute("target".into()),
    ])];
    let desired =
        crate::resource::ResolvedResource::resolve_raw_fixture(desired, &schemas).unwrap();
    let previous = State::existing(
        desired.id.clone(),
        crate::test_support::snapshot_attributes(HashMap::from([
            ("guard".into(), Value::Concrete(ConcreteValue::Bool(false))),
            ("target".into(), string("other")),
        ])),
    )
    .with_identifier("existing");
    let mut plan = Plan::new();
    plan.add(Effect::Update {
        from: Box::new(previous),
        identifier: AppliedIdentifier::new("existing").unwrap(),
        to: desired.clone(),
        changed_attributes: vec!["guard".into()],
    });
    let provider = MockProvider::new();
    provider.push_update(Ok(ok_state(&desired.id)));
    let (result, events) = run(
        &provider,
        plan,
        &schemas,
        HashMap::new(),
        Default::default(),
        &NoopNormalizer,
    )
    .await;
    assert_eq!(
        (result.success_count, result.failure_count()),
        (0, 1),
        "{events:?}"
    );
    assert!(provider.captured_update_requests().is_empty());
    assert!(
        matches!(result.failures()[0].kind(), EffectFailureKind::UnresolvedInputRule { path, .. } if path.as_str() == "target")
    );
}

#[tokio::test]
async fn input_discharge_update_only_checks_sent_paths() {
    for write_only in [false, true] {
        let mut schemas = SchemaRegistry::new();
        let name = PatternAttributeSchema::optional("name", PatternAttributeType::string());
        schemas.insert(
            "",
            PatternResourceSchema::new("test")
                .attribute(if write_only {
                    name.write_only().build()
                } else {
                    name.build()
                })
                .attribute(
                    PatternAttributeSchema::optional(
                        "tags",
                        PatternAttributeType::map(PatternAttributeType::string()),
                    )
                    .build(),
                )
                .input_rule(InputRule::value(
                    InputRuleScope::EveryWrite,
                    InputPath::new("name").unwrap(),
                    InputRuleCode::new("test.name.not_ipv4").unwrap(),
                    InputPredicate::NotIpv4,
                ))
                .validate()
                .unwrap(),
        );
        let resource = resolved_fixture_resources(vec![
            Resource::new("test", "patch")
                .with_attribute("name", string("192.168.1.1"))
                .with_attribute(
                    "tags",
                    Value::Concrete(ConcreteValue::Map(indexmap::IndexMap::from([(
                        "safe".into(),
                        string("updated"),
                    )]))),
                ),
        ])
        .remove(0);
        let from = State::existing(
            resource.id.clone(),
            crate::test_support::snapshot_attributes(HashMap::from([
                ("name".into(), string("192.168.1.1")),
                (
                    "tags".into(),
                    Value::Concrete(ConcreteValue::Map(indexmap::IndexMap::from([(
                        "safe".into(),
                        string("old"),
                    )]))),
                ),
            ])),
        )
        .with_identifier("old-id");
        let mut plan = Plan::new();
        plan.add(Effect::Update {
            from: Box::new(from.clone()),
            identifier: AppliedIdentifier::new("old-id").unwrap(),
            to: resource.clone(),
            changed_attributes: vec!["tags".into()],
        });
        let admission = FinalizedPlan::new(plan.clone()).check_inputs(&schemas, &HashMap::new());
        if write_only {
            assert!(
                matches!(
                    admission.unwrap_err().errors()[0].kind,
                    crate::plan::PlanErrorKind::InputRule { .. }
                ),
                "Reassert sends the unchanged name"
            );
        } else {
            assert!(admission.unwrap().pending_inputs().is_empty());
            let provider = MockProvider::new();
            provider.push_update(Ok(ok_state(&resource.id)));
            let (result, events) = run(
                &provider,
                plan,
                &schemas,
                HashMap::new(),
                Default::default(),
                &NoopNormalizer,
            )
            .await;
            assert_eq!(
                (result.success_count, result.failure_count()),
                (1, 0),
                "{events:?}"
            );
            let requests = provider.captured_update_requests();
            assert_eq!(requests.len(), 1);
            assert_eq!(requests[0].patch().ops.len(), 1);
            assert_eq!(requests[0].patch().ops[0].key(), "tags");
        }
        let mut plan = Plan::new();
        plan.add(Effect::Update {
            from: Box::new(from),
            identifier: AppliedIdentifier::new("old-id").unwrap(),
            to: resource,
            changed_attributes: vec!["name".into()],
        });
        assert!(matches!(
            FinalizedPlan::new(plan)
                .check_inputs(&schemas, &HashMap::new())
                .unwrap_err()
                .errors()[0]
                .kind,
            crate::plan::PlanErrorKind::InputRule { .. }
        ));
    }
}

fn loop_effect(replace: bool) -> Effect {
    let template = crate::parser::DeferredForExpression::new(
        Some("main.crn".into()),
        1,
        "for item in source.items".into(),
        crate::parser::DeferredForTarget::bound(absolute_binding("children")).unwrap(),
        Value::root_resource_ref("source", "items", vec![]),
        crate::parser::ForBinding::Simple("item".into()),
        crate::resource::Resource::new("test", "children").with_attribute(
            "name",
            crate::resource::Value::Deferred(crate::resource::DeferredValue::Placeholder(
                LoopSlot::Value,
            )),
        ),
    );
    let payload =
        crate::test_support::compiled_loop_with_producers(Box::new(template), vec![source()]);
    if !replace {
        return Effect::DeferredCreate(payload);
    }
    Effect::DeferredReplace(Box::new(crate::effect::DeferredReplacePayload::new(
        NonEmptyDeletes::try_new(
            (0..2)
                .map(|index| DeferredReplaceDelete {
                    id: test_resolved_id("test", &format!("children[{index}]")),
                    generation: EffectGeneration::Current,
                    previous_attributes: HashMap::new(),
                    identifier: AppliedIdentifier::new(format!("old-{index}")).unwrap(),
                    directives: Default::default(),
                    binding: Some(absolute_binding(&format!("children[{index}]"))),
                    dependencies: Default::default(),
                    explicit_dependencies: Default::default(),
                    blocked_by_updates: Default::default(),
                })
                .collect(),
        )
        .unwrap(),
        payload.template,
        payload.dependency_template,
    )))
}

#[tokio::test]
async fn input_discharge_deferred_cohort_is_checked_before_children_or_absorbed_deletes() {
    for replace in [false, true] {
        for values in [
            vec!["valid-child", "192.168.123.234"],
            vec![],
            vec!["valid-child", "another-child"],
        ] {
            let schemas = schemas(InputRuleScope::CreateOnly);
            let provider = MockProvider::new();
            let mut bindings = ResolvedBindings::default();
            bindings.set_with_completeness(
                "source",
                HashMap::from([(
                    "items".into(),
                    Value::Concrete(ConcreteValue::List(
                        values.iter().map(|v| string(v)).collect(),
                    )),
                )]),
                crate::binding_index::BindingValueSource::Local,
                crate::binding_index::BindingCompleteness::Complete,
            );
            let mut plan = Plan::new();
            plan.add(loop_effect(replace));
            let mut current = HashMap::new();
            let valid = values.iter().all(|value| *value != "192.168.123.234");
            if valid {
                for index in 0..values.len() {
                    provider.push_create(Ok(ok_state(
                        test_resolved_id("test", &format!("children[{index}]")).as_inner(),
                    )));
                }
            }
            if replace {
                for index in 0..2 {
                    let id = test_resolved_id("test", &format!("children[{index}]"))
                        .as_inner()
                        .clone();
                    current.insert(
                        id.clone(),
                        Current::from_snapshot(
                            ok_state(&id).with_identifier(format!("old-{index}")),
                        ),
                    );
                    provider.push_delete(Ok(()));
                }
            }
            let (result, events) = run(
                &provider,
                plan,
                &schemas,
                current.clone(),
                bindings,
                &NoopNormalizer,
            )
            .await;
            if valid {
                assert_eq!(result.failure_count(), 0, "{events:?}");
                assert_eq!(
                    provider.calls().len(),
                    values.len() + if replace { 2 } else { 0 }
                );
                assert_eq!(
                    provider
                        .captured_create_resources()
                        .iter()
                        .map(|resource| resource.attributes["name"].clone())
                        .collect::<Vec<_>>(),
                    values.iter().map(|value| string(value)).collect::<Vec<_>>()
                );
            } else {
                assert_eq!(result.failure_count(), 1, "{events:?}");
                assert_eq!(result.failures()[0].kind().label(), "input rule violation");
                assert!(
                    provider.calls().is_empty(),
                    "the entire cohort must be prepared first"
                );
                assert_eq!(result.managed_current, current);
                assert!(!format!("{:?} {events:?}", result.failures()).contains("192.168.123.234"));
            }
        }
    }
}

#[tokio::test]
async fn input_discharge_failed_prerequisite_preserves_old_resource() {
    let schemas = schemas(InputRuleScope::CreateOnly);
    let mut resources = resolved_fixture_resources(vec![source(), consumer()]);
    let old = Current::from_snapshot(ok_state(&resources[1].id).with_identifier("old-id"));
    let mut plan = Plan::new();
    plan.add(Effect::Create(resources.remove(0)));
    replacement(&mut plan, resources.remove(0), false);
    let provider = MockProvider::new();
    provider.push_create(Err(ProviderError::internal("prerequisite failed")));
    let (result, events) = run(
        &provider,
        plan,
        &schemas,
        HashMap::from([(consumer().id, old.clone())]),
        Default::default(),
        &NoopNormalizer,
    )
    .await;
    assert_eq!(
        (
            result.success_count,
            result.failure_count(),
            result.skip_count
        ),
        (0, 1, 2),
        "{events:?}"
    );
    assert_eq!(
        provider.calls(),
        vec![("create".into(), source().id.to_string())]
    );
    assert_eq!(result.managed_current.get(&consumer().id), Some(&old));
}

#[tokio::test]
async fn input_discharge_resume_unissued_checks_and_issued_skips() {
    for (issued, value) in [
        (false, "192.168.123.234"),
        (true, "192.168.123.234"),
        (false, "valid-resume"),
    ] {
        let schemas = schemas(InputRuleScope::CreateOnly);
        let provider = MockProvider::new();
        let resource = consumer();
        let pending = if issued {
            persisted_create_for_executor(&resource.id)
        } else {
            persisted_unissued_create_for_executor(&resource.id)
        };
        let mut bindings = ResolvedBindings::default();
        bindings.set_with_completeness(
            "source",
            HashMap::from([("value".into(), string(value))]),
            crate::binding_index::BindingValueSource::Local,
            crate::binding_index::BindingCompleteness::Complete,
        );
        let resources = resolved_fixture_resources(vec![source(), resource]);
        let mut plan = Plan::new();
        plan.add_resume_create(resources[1].clone(), pending.started_at().clone());
        if issued {
            provider.push_resume(ResumeFixture::Resumed);
        } else if value == "valid-resume" {
            provider.push_resume(ResumeFixture::Resent);
        }
        let (result, events) = run(
            &provider,
            plan,
            &schemas,
            HashMap::from([(resources[1].id.clone(), Current::CreateInFlight(pending))]),
            bindings,
            &NoopNormalizer,
        )
        .await;
        let accepted = issued || value == "valid-resume";
        assert_eq!(result.success_count, usize::from(accepted), "{events:?}");
        assert_eq!(result.failure_count(), usize::from(!accepted), "{events:?}");
        if !accepted {
            assert!(matches!(
                result.failures()[0].kind(),
                EffectFailureKind::InputRule { .. }
            ));
        }
        if value == "valid-resume" {
            assert_eq!(
                provider.calls(),
                vec![("create".into(), consumer().id.to_string())]
            );
            assert_eq!(
                provider.captured_create_resources()[0].attributes["name"],
                string(value)
            );
        }
    }
}

#[tokio::test]
async fn input_discharge_default_tags_are_checked_in_the_sent_projection() {
    for key in ["ManagedBy", "aws:forbidden"] {
        let mut schemas = SchemaRegistry::new();
        schemas.insert(
            "mock",
            PatternResourceSchema::new("Bucket")
                .attribute(
                    PatternAttributeSchema::optional(
                        "tags",
                        PatternAttributeType::map(PatternAttributeType::string()),
                    )
                    .build(),
                )
                .input_rule(InputRule::new(
                    InputRuleScope::EveryWrite,
                    crate::schema::InputRuleTarget::MapKeys(InputPath::new("tags").unwrap()),
                    InputRuleCode::new("test.tags.reserved_prefix").unwrap(),
                    InputPredicate::ForbiddenPrefix {
                        prefix: crate::schema::NonEmptyString::new("aws:").unwrap(),
                        ignore_ascii_case: true,
                    },
                ))
                .validate()
                .unwrap(),
        );
        let target = Resource::with_provider("mock", "Bucket", "tagged", None)
            .with_attribute("tags", Value::root_resource_ref("source", "value", vec![]));
        let resources = resolved_fixture_resources(vec![source(), target]);
        let provider = MockProvider::new();
        provider.push_create(Ok(State::existing(
            resources[0].id.clone(),
            crate::test_support::snapshot_attributes(HashMap::from([(
                "value".into(),
                Value::Concrete(ConcreteValue::Map(indexmap::IndexMap::from([(
                    "safe".into(),
                    string("authored"),
                )]))),
            )])),
        )
        .with_identifier("source-id")));
        provider.push_create(Ok(ok_state(&resources[1].id)));
        let mut plan = Plan::new();
        for resource in resources {
            plan.add(Effect::Create(resource));
        }
        let mut config = provider_config_with_default_tags(indexmap::IndexMap::from([(
            key.into(),
            string("default"),
        )]));
        config.name = "mock".into();
        let (result, events) = run_with_configs(
            &provider,
            plan,
            &schemas,
            HashMap::new(),
            Default::default(),
            &NoopNormalizer,
            &[config],
        )
        .await;
        if key == "ManagedBy" {
            assert_eq!(
                (result.success_count, result.failure_count()),
                (2, 0),
                "{events:?}"
            );
            let captured = provider.captured_create_resources();
            let Value::Concrete(ConcreteValue::Map(tags)) = &captured[1].attributes["tags"] else {
                panic!("merged tags")
            };
            assert_eq!(tags[key], string("default"));
            assert_eq!(tags["safe"], string("authored"));
        } else {
            assert_eq!(
                (result.success_count, result.failure_count()),
                (1, 1),
                "{events:?}"
            );
            assert!(
                matches!(result.failures()[0].kind(), EffectFailureKind::InputRule { violation } if violation.path().as_str() == "tags")
            );
            assert!(!format!("{:?} {events:?}", result.failures()).contains(key));
        }
    }
}

#[tokio::test]
async fn input_discharge_genuine_preparation_cycle_releases_no_delete() {
    let schemas = schemas(InputRuleScope::CreateOnly);
    let mut parent = Resource::new("test", "parent")
        .with_attribute("name", Value::root_resource_ref("child", "name", vec![]));
    parent.binding = Some(absolute_binding("parent"));
    parent
        .dependencies
        .value_bindings
        .insert(absolute_binding("child"));
    let mut child = Resource::new("test", "child")
        .with_attribute("name", Value::root_resource_ref("parent", "name", vec![]));
    child.binding = Some(absolute_binding("child"));
    let resources = resolved_fixture_resources(vec![parent, child]);
    let mut plan = Plan::new();
    replacement(&mut plan, resources[0].clone(), false);
    // Main resolves historical replacement-order cycles at admission. Keep a
    // genuine value/preparation cycle, which changing replacement order cannot break.
    replacement(&mut plan, resources[1].clone(), false);
    let current: HashMap<_, _> = resources
        .iter()
        .map(|resource| {
            (
                resource.id.clone(),
                Current::from_snapshot(ok_state(&resource.id)),
            )
        })
        .collect();
    let provider = MockProvider::new();
    let (result, events) = run(
        &provider,
        plan,
        &schemas,
        current.clone(),
        Default::default(),
        &NoopNormalizer,
    )
    .await;
    assert!(
        result
            .failures()
            .iter()
            .any(|failure| matches!(failure.kind(), EffectFailureKind::DependencyCycle { .. })),
        "{events:?}"
    );
    assert!(provider.calls().is_empty());
    assert_eq!(result.managed_current, current);
    assert_eq!(
        result.failure_count(),
        4,
        "internal preparation must not produce duplicate failures: {events:?}"
    );
}

#[tokio::test]
async fn input_discharge_failed_cohort_preparation_advances_progress() {
    struct ProgressObserver(Mutex<Vec<usize>>);
    impl ExecutionObserver for ProgressObserver {
        fn on_event(&self, event: &ExecutionEvent) {
            if let ExecutionEvent::EffectFailed { progress, .. } = event {
                self.0.lock().unwrap().push(progress.completed);
            }
        }
    }
    for replace in [false, true] {
        let schemas = schemas(InputRuleScope::CreateOnly);
        let mut plan = Plan::new();
        plan.add(loop_effect(replace));
        let checked = FinalizedPlan::new(plan)
            .check_inputs(&schemas, &HashMap::new())
            .unwrap();
        let mut bindings = ResolvedBindings::default();
        bindings.set_with_completeness(
            "source",
            HashMap::from([(
                "items".into(),
                Value::Concrete(ConcreteValue::List(vec![string("192.168.1.1")])),
            )]),
            crate::binding_index::BindingValueSource::Local,
            crate::binding_index::BindingCompleteness::Complete,
        );
        let origins = HashMap::from([(
            ResolvedResourceId::new(source().id.clone()),
            UnresolvedResource::from_pre_resolve(source()),
        )]);
        let observer = ProgressObserver(Mutex::new(vec![]));
        let provider = MockProvider::new();
        let result = completed_result(
            execute_plan(
                &provider,
                ExecutionInput {
                    plan: &checked,
                    provider_context: crate::parser::ProviderContext::clone(
                        &crate::parser::LoadedProviderContext::empty_for_test(),
                    ),
                    provider_check_inputs: ProviderCheckInputs::PlanNormalized {
                        resource_origins: &origins,
                        data_source_origins: &[],
                    },
                    compositions: &[],
                    bindings,
                    managed_current: HashMap::new(),
                    data_source_values: ExecutableDataSourceValues::empty(),
                    normalizer: &NoopNormalizer,
                    provider_configs: &[],
                    factories: &[],
                    schemas: &schemas,
                    parallelism: NonZeroUsize::new(8).unwrap(),
                    precompleted_success_count: 0,
                    checkpoint_sink: ExecutionCheckpointSink::noop(),
                },
                &observer,
                uncancelled_shutdown(),
            )
            .await,
        );
        assert_eq!(result.failure_count(), 1);
        assert_eq!(result.failures()[0].kind().label(), "input rule violation");
        assert!(provider.calls().is_empty());
        assert_eq!(
            *observer.0.lock().unwrap(),
            [1],
            "the failed template counts as completed"
        );
    }
}

#[tokio::test]
async fn input_discharge_invalid_check_construction_aborts_all_dispatch() {
    use crate::executor::input_discharge::InputCheckConstructionError;
    let schemas = schemas(InputRuleScope::CreateOnly);
    let resources = resolved_fixture_resources(vec![
        Resource::new("test", "first"),
        Resource::new("test", "second"),
    ]);
    let mut plan = Plan::new();
    for resource in resources {
        plan.add(Effect::Create(resource));
    }
    let checked = FinalizedPlan::new(plan)
        .check_inputs(&schemas, &HashMap::new())
        .unwrap();
    let provider = MockProvider::new();
    let observer = MockObserver::new();
    let result = crate::validation::deferred_types::with_session_for_test(
        &crate::parser::LoadedProviderContext::empty_for_test(),
        |session| {
            let mut input = ExecutionInput {
                plan: &checked,
                provider_context: crate::parser::ProviderContext::clone(
                    &crate::parser::LoadedProviderContext::empty_for_test(),
                ),
                provider_check_inputs: ProviderCheckInputs::Authored,
                compositions: &[],
                bindings: Default::default(),
                managed_current: HashMap::new(),
                data_source_values: ExecutableDataSourceValues::empty(),
                normalizer: &NoopNormalizer,
                provider_configs: &[],
                factories: &[],
                schemas: &schemas,
                parallelism: NonZeroUsize::new(8).unwrap(),
                precompleted_success_count: 0,
                checkpoint_sink: ExecutionCheckpointSink::noop(),
            }
            .in_session(session);
            input.input_checks.push(input.input_checks[1].clone());
            futures::executor::block_on(parallel::execute_effects_sequential(
                &crate::provider::ProviderBoundary::fixture(&provider),
                &crate::provider::ProviderRouter::new(),
                &mut input,
                &observer,
                &uncancelled_shutdown(),
            ))
            .0
        },
    );
    assert_eq!((result.success_count, result.failure_count()), (0, 1));
    assert!(provider.calls().is_empty());
    let EffectFailureKind::Execution { error } = result.failures()[0].kind() else {
        panic!("typed execution failure required")
    };
    assert!(matches!(
        error
            .as_error()
            .downcast_ref::<InputCheckConstructionError>(),
        Some(InputCheckConstructionError::DuplicateWrite { .. })
    ));
}

#[test]
fn round1_rule_bearing_template_keeps_unrelated_delete_and_non_input_edges() {
    let schemas = schemas(InputRuleScope::CreateOnly);
    let mut plan = Plan::new();
    plan.add(loop_effect(true));
    let checked = FinalizedPlan::new(plan)
        .check_inputs(&schemas, &HashMap::new())
        .unwrap();
    let scheduled = checked.schedule(&HashMap::new());
    let (scheduled, _, sources) = scheduled.expand_deferred_replacements();
    let mut effects = scheduled.effects().to_vec();
    let checks = super::super::input_discharge::ExecutionInputChecks::new(
        checked.execution_input_checks(),
        &effects,
        &sources,
    )
    .unwrap();
    let template = effects
        .iter()
        .position(|effect| matches!(effect, Effect::DeferredReplace(_)))
        .unwrap();
    let own_deletes = checks.paired[&template].clone();
    let nodes = super::super::scheduler::PreparationNodes::install(&mut effects, &checks);
    // A distinct slot can even have the same routed ID as an absorbed delete.
    // Neither its name nor Effect::Delete alone makes it part of the cycle.
    let unrelated = effects.len();
    let mut other = effects[own_deletes[0]].clone();
    if let Effect::Delete { identifier, .. } = &mut other {
        *identifier = AppliedIdentifier::new("unrelated-generation").unwrap();
    }
    effects.push(other);
    let non_input = effects.len();
    effects.push(Effect::Create(resolved(Resource::new(
        "test",
        "explicit-prerequisite",
    ))));
    let mut deps = HashMap::from([
        (
            template,
            own_deletes
                .iter()
                .copied()
                .chain([unrelated, non_input])
                .collect(),
        ),
        (own_deletes[0], HashSet::from([template])),
        (own_deletes[1], HashSet::from([template])),
        (unrelated, HashSet::new()),
        (non_input, HashSet::new()),
    ]);
    nodes.dependencies(&effects, &mut deps);
    assert_eq!(deps[&template], HashSet::from([unrelated, non_input]));
    assert!(deps[&own_deletes[0]].contains(&template));
    assert!(deps[&own_deletes[1]].contains(&template));
}

#[tokio::test]
async fn round1_unbound_rule_bearing_replacement_accepts_deleted_anonymous_child_identity() {
    let (template, bindings, _, child) = anonymous_deferred_collision_fixture();
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        PatternResourceSchema::new("test.Child")
            .with_conflicting_replacement()
            .attribute(
                PatternAttributeSchema::optional("identity_input", PatternAttributeType::string())
                    .create_only()
                    .build(),
            )
            .input_rule(InputRule::value(
                InputRuleScope::CreateOnly,
                InputPath::new("identity_input").unwrap(),
                InputRuleCode::new("child.not_ipv4").unwrap(),
                InputPredicate::NotIpv4,
            ))
            .validate()
            .unwrap(),
    );
    let payload = crate::test_support::compiled_loop(Box::new(template));
    let old_absorbed = DeferredReplaceDelete {
        id: ResolvedResourceId::with_provider_identity("mock", "test.Child", "removed-child", None),
        identifier: AppliedIdentifier::new("removed-child-id").unwrap(),
        generation: EffectGeneration::Current,
        directives: Default::default(),
        binding: None,
        dependencies: Default::default(),
        explicit_dependencies: Default::default(),
        blocked_by_updates: Default::default(),
        previous_attributes: Default::default(),
    };
    let mut plan = Plan::new();
    plan.add(Effect::DeferredReplace(Box::new(
        crate::effect::DeferredReplacePayload::new(
            NonEmptyDeletes::try_new(vec![old_absorbed.clone()]).unwrap(),
            payload.template,
            payload.dependency_template,
        ),
    )));
    plan.add_delete(DeferredReplaceDelete {
        id: ResolvedResourceId::new(child.id.clone()),
        identifier: AppliedIdentifier::new("anonymous-old-id").unwrap(),
        ..old_absorbed.clone()
    });
    let current = HashMap::from([
        (
            child.id.clone(),
            Current::from_snapshot(ok_state(&child.id).with_identifier("anonymous-old-id")),
        ),
        (
            old_absorbed.id.as_inner().clone(),
            Current::from_snapshot(
                ok_state(old_absorbed.id.as_inner()).with_identifier("removed-child-id"),
            ),
        ),
    ]);
    let checked = FinalizedPlan::new(plan)
        .check_inputs(&schemas, &current)
        .unwrap();
    let provider = MockProvider::new();
    provider.push_delete(Ok(()));
    provider.push_delete(Ok(()));
    provider.push_create(Ok(ok_state(&child.id)));
    let input = ExecutionInput {
        provider_context: crate::parser::LoadedProviderContext::empty_for_test()
            .as_provider_context()
            .clone(),
        plan: &checked,
        provider_check_inputs: ProviderCheckInputs::Authored,
        compositions: &[],
        bindings,
        managed_current: current,
        data_source_values: ExecutableDataSourceValues::empty(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &schemas,
        parallelism: NonZeroUsize::new(1).unwrap(),
        precompleted_success_count: 0,
        checkpoint_sink: ExecutionCheckpointSink::noop(),
    };
    let observer = MockObserver::new();
    let result =
        completed_result(execute_plan(&provider, input, &observer, uncancelled_shutdown()).await);
    assert_eq!(
        (result.success_count, result.failure_count()),
        (3, 0),
        "{:?}",
        observer.events()
    );
    let calls = provider.calls();
    assert!(
        call_position(&calls, "delete", &child.id.to_string())
            < call_position(&calls, "create", &child.id.to_string()),
        "{calls:?}"
    );
}

#[tokio::test]
async fn input_discharge_conflicting_child_in_cbd_parent_window_keeps_old_on_rejection() {
    for valid in [false, true] {
        let mut schemas = schemas_with_order(InputRuleScope::CreateOnly, false);
        schemas.insert(
            "",
            PatternResourceSchema::new("Parent")
                .with_coexisting_replacement()
                .attribute(
                    PatternAttributeSchema::provider_populated(
                        "items",
                        PatternAttributeType::list(PatternAttributeType::string()),
                    )
                    .build(),
                )
                .validate()
                .unwrap(),
        );
        let parent =
            Resource::new("Parent", "source").with_absolute_binding(absolute_binding("source"));
        let parent = ResolvedResource::resolve_raw_fixture(parent.clone(), &schemas).unwrap();
        let template = crate::parser::DeferredForExpression::new(
            Some("main.crn".into()),
            1,
            "for item in source.items".into(),
            crate::parser::DeferredForTarget::bound(absolute_binding("children")).unwrap(),
            Value::root_resource_ref("source", "items", vec![]),
            crate::parser::ForBinding::Simple("item".into()),
            crate::resource::Resource::new("test", "children").with_attribute(
                "name",
                crate::resource::Value::Deferred(crate::resource::DeferredValue::Placeholder(
                    LoopSlot::Value,
                )),
            ),
        );
        let payload = crate::test_support::compiled_loop_with_producers(
            Box::new(template),
            vec![
                parent
                    .as_inner()
                    .clone()
                    .with_dependency_phase(Default::default()),
            ],
        );
        let child_id = test_resolved_id("test", "children[0]");
        let old = DeferredReplaceDelete {
            id: child_id.clone(),
            identifier: AppliedIdentifier::new("old-child").unwrap(),
            generation: EffectGeneration::Current,
            directives: Default::default(),
            binding: Some(absolute_binding("children[0]")),
            dependencies: crate::test_support::persisted([parent.resolved_id()]),
            explicit_dependencies: Default::default(),
            blocked_by_updates: Default::default(),
            previous_attributes: Default::default(),
        };
        let mut plan = Plan::new();
        replacement(&mut plan, parent.clone(), true);
        plan.add(Effect::DeferredReplace(Box::new(
            crate::effect::DeferredReplacePayload::new(
                NonEmptyDeletes::try_new(vec![old]).unwrap(),
                payload.template,
                payload.dependency_template,
            ),
        )));
        let current = HashMap::from([
            (
                parent.id.clone(),
                Current::from_snapshot(ok_state(&parent.id).with_identifier("old-id")),
            ),
            (
                child_id.as_inner().clone(),
                Current::from_snapshot(ok_state(child_id.as_inner()).with_identifier("old-child")),
            ),
        ]);
        let checked = FinalizedPlan::new(plan)
            .check_inputs(&schemas, &current)
            .unwrap();
        let provider = MockProvider::new();
        provider.push_create(Ok(State::existing(
            parent.id.clone(),
            crate::test_support::snapshot_attributes(HashMap::from([(
                "items".into(),
                Value::Concrete(ConcreteValue::List(vec![string(if valid {
                    "valid-child"
                } else {
                    "192.168.123.234"
                })])),
            )])),
        )
        .with_identifier("new-parent")));
        if valid {
            provider.push_delete(Ok(()));
            provider.push_delete(Ok(()));
            provider.push_create(Ok(ok_state(child_id.as_inner())));
        }
        let observer = MockObserver::new();
        let input = ExecutionInput {
            provider_context: crate::parser::LoadedProviderContext::empty_for_test()
                .as_provider_context()
                .clone(),
            plan: &checked,
            provider_check_inputs: ProviderCheckInputs::Authored,
            compositions: &[],
            bindings: ResolvedBindings::default(),
            managed_current: current,
            data_source_values: ExecutableDataSourceValues::empty(),
            normalizer: &NoopNormalizer,
            provider_configs: &[],
            factories: &[],
            schemas: &schemas,
            parallelism: NonZeroUsize::new(1).unwrap(),
            precompleted_success_count: 0,
            checkpoint_sink: ExecutionCheckpointSink::noop(),
        };
        let result = completed_result(
            execute_plan(&provider, input, &observer, uncancelled_shutdown()).await,
        );
        let calls = provider.calls();
        if valid {
            assert_eq!(
                (result.success_count, result.failure_count()),
                (4, 0),
                "{:?}",
                observer.events()
            );
            let ordered = [
                call_position(&calls, "create", &parent.id.to_string()),
                call_position(&calls, "delete", &child_id.to_string()),
                call_position(&calls, "create", &child_id.to_string()),
                call_position(&calls, "delete", &parent.id.to_string()),
            ];
            assert!(
                ordered.windows(2).all(|pair| pair[0] < pair[1]),
                "{calls:?}"
            );
        } else {
            assert_eq!(
                (result.success_count, result.failure_count()),
                (1, 1),
                "{:?}",
                observer.events()
            );
            assert!(matches!(
                result.failures()[0].kind(),
                EffectFailureKind::InputRule { .. }
            ));
            assert_eq!(calls, vec![("create".into(), parent.id.to_string())]);
            assert!(result.successfully_deleted.is_empty());
        }
    }
}

#[tokio::test]
async fn input_discharge_identifierless_conflicting_resume_checks_before_old_delete() {
    for valid in [false, true] {
        let schemas = schemas_with_order(InputRuleScope::CreateOnly, false);
        let mut resources = resolved_fixture_resources(vec![source(), consumer()]);
        let target = resources.remove(1);
        let pending = persisted_unissued_create_for_executor(&target.id);
        let mut plan = Plan::new();
        plan.add(Effect::Create(resources.remove(0)));
        plan.add_resume_create(target.clone(), pending.started_at().clone());
        plan.add_delete(DeferredReplaceDelete {
            id: target.resolved_id(),
            identifier: AppliedIdentifier::new("old-id").unwrap(),
            generation: EffectGeneration::Deposed(crate::resource::DeposedKey::new_unique()),
            directives: Default::default(),
            binding: target.binding.clone(),
            dependencies: Default::default(),
            explicit_dependencies: Default::default(),
            blocked_by_updates: Default::default(),
            previous_attributes: Default::default(),
        });
        let provider = MockProvider::new();
        provider.push_create(Ok(State::existing(
            source().id,
            crate::test_support::snapshot_attributes(HashMap::from([(
                "value".into(),
                string(if valid {
                    "valid-name"
                } else {
                    "192.168.123.234"
                }),
            )])),
        )
        .with_identifier("source-id")));
        if valid {
            provider.push_delete(Ok(()));
            provider.push_resume(ResumeFixture::Resent);
        }
        let (result, events) = run(
            &provider,
            plan,
            &schemas,
            HashMap::from([(target.id.clone(), Current::CreateInFlight(pending))]),
            Default::default(),
            &NoopNormalizer,
        )
        .await;
        let calls = provider.calls();
        if valid {
            assert_eq!(
                (result.success_count, result.failure_count()),
                (3, 0),
                "{events:?}"
            );
            assert!(
                call_position(&calls, "delete", &target.id.to_string())
                    < call_position(&calls, "create", &target.id.to_string()),
                "{calls:?}"
            );
            assert_eq!(
                provider.captured_create_resources()[1].attributes["name"],
                string("valid-name")
            );
        } else {
            assert_eq!(
                (result.success_count, result.failure_count()),
                (1, 1),
                "{events:?}"
            );
            assert!(matches!(
                result.failures()[0].kind(),
                EffectFailureKind::InputRule { .. }
            ));
            assert_eq!(calls, vec![("create".into(), source().id.to_string())]);
            assert!(result.successfully_deleted.is_empty());
        }
    }
}

struct ActivatingConditionalNormalizer;
impl crate::provider::ProviderNormalizer for ActivatingConditionalNormalizer {
    fn normalize_desired<'a>(
        &'a self,
        resources: &'a mut [crate::resource::ProviderResource],
    ) -> BoxFuture<'a, ProviderResult<()>> {
        Box::pin(async move {
            for resource in resources {
                resource.set_attr("enabled", Value::Concrete(ConcreteValue::Bool(true)));
                resource.set_attr("name", string("192.168.1.1"));
            }
            Ok(())
        })
    }
    fn normalize_state<'a>(
        &'a self,
        _: &'a mut crate::provider::StateTransformMap,
    ) -> BoxFuture<'a, ProviderResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn hydrate_read_state<'a>(
        &'a self,
        _: &'a mut crate::provider::StateTransformMap,
        _: &'a crate::provider::SavedAttrs,
    ) -> BoxFuture<'a, ProviderResult<()>> {
        Box::pin(async { Ok(()) })
    }
}

#[tokio::test]
async fn conditional_rules_recheck_normalized_guards_at_create_and_update_dispatch() {
    use crate::schema::{InputCondition, InputLiteral};
    for scope in [InputRuleScope::CreateOnly, InputRuleScope::EveryWrite] {
        for update in [false, true] {
            let mut schemas = SchemaRegistry::new();
            schemas.insert(
                "",
                PatternResourceSchema::new("test")
                    .attribute(
                        PatternAttributeSchema::optional("name", PatternAttributeType::string())
                            .build(),
                    )
                    .attribute(
                        PatternAttributeSchema::optional("enabled", PatternAttributeType::bool())
                            .build(),
                    )
                    .input_rule(
                        InputRule::value(
                            scope,
                            InputPath::new("name").unwrap(),
                            InputRuleCode::new("test.conditional_name").unwrap(),
                            InputPredicate::NotIpv4,
                        )
                        .with_condition(InputCondition::Equals {
                            path: InputPath::new("enabled").unwrap(),
                            literal: InputLiteral::Bool(true),
                        }),
                    )
                    .validate()
                    .unwrap(),
            );
            let resource = resolved_fixture_resources(vec![
                Resource::new("test", "conditional")
                    .with_attribute("name", string("valid-name"))
                    .with_attribute("enabled", Value::Concrete(ConcreteValue::Bool(false))),
            ])
            .remove(0);
            let provider = MockProvider::new();
            let mut plan = Plan::new();
            if update {
                provider.push_update(Ok(ok_state(&resource.id)));
                plan.add(Effect::Update {
                    from: Box::new(ok_state(&resource.id)),
                    identifier: AppliedIdentifier::new("existing").unwrap(),
                    to: resource,
                    changed_attributes: vec!["name".into(), "enabled".into()],
                });
            } else {
                provider.push_create(Ok(ok_state(&resource.id)));
                plan.add(Effect::Create(resource));
            }
            let (result, events) = run(
                &provider,
                plan,
                &schemas,
                HashMap::new(),
                Default::default(),
                &ActivatingConditionalNormalizer,
            )
            .await;
            let rejected = !update || scope == InputRuleScope::EveryWrite;
            assert_eq!(
                result.failure_count(),
                usize::from(rejected),
                "{scope:?}, update={update}: {events:?}"
            );
            if rejected {
                assert!(provider.calls().is_empty());
                assert!(
                    matches!(result.failures()[0].kind(),EffectFailureKind::InputRule { violation } if violation.code().as_str() == "test.conditional_name")
                );
            } else {
                assert_eq!(result.success_count, 1, "{events:?}");
            }
        }
    }
}
