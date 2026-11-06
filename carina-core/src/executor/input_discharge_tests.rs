//! Phase 4 regressions. These deliberately use real admission and scheduling.
use super::*;
use crate::input_rules::FinalizedPlan;
use crate::schema::{InputPath, InputPredicate, InputRule, InputRuleCode, InputRuleScope};

fn schemas(scope: InputRuleScope) -> SchemaRegistry {
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "",
        PatternResourceSchema::new("test")
            .attribute(
                PatternAttributeSchema::optional("name", PatternAttributeType::string()).build(),
            )
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
    plan.add_replacement(crate::plan::ReplacementGroup {
        delete: crate::plan::ReplacementDelete {
            id: create.resolved_id(),
            identifier: AppliedIdentifier::new("old-id").unwrap(),
            directives: Default::default(),
            binding: create.binding.clone(),
            dependencies,
            explicit_dependencies: Default::default(),
        },
        create,
        create_before_destroy: cbd,
        changed_create_only: crate::effect::ChangedCreateOnly::new(vec!["generation".into()])
            .unwrap(),
        cascade_ref_hints: vec![],
        temporary_name: None,
        permanent_name_override: None,
        consumer_updates: Default::default(),
        previous_attributes: HashMap::new(),
    });
}

struct ChangingNormalizer {
    calls: AtomicUsize,
    first: &'static str,
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
        if let Some(basic) = effect.as_basic() {
            let resource = match basic {
                crate::effect::BasicEffect::Create { resource, .. }
                | crate::effect::BasicEffect::ResumeCreate { resource, .. } => Some(resource),
                crate::effect::BasicEffect::Update { to, .. } => Some(to),
                crate::effect::BasicEffect::Delete { .. } => None,
            };
            if let Some(resource) = resource {
                origins.insert(
                    resource.resolved_id(),
                    UnresolvedResource::from_pre_resolve(resource.as_inner().clone()),
                );
            }
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
            normalizer,
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
    // The identical ResourceId does not make this a replacement. Only
    // admitted replacement slot metadata can pair a delete with a create.
    plan.add(Effect::Delete {
        id: ResolvedResourceId::new(consumer().id),
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
            .contains(&("delete".into(), consumer().id.to_string()))
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
    Effect::DeferredReplace(Box::new(crate::effect::DeferredReplacePayload {
        deletes: NonEmptyDeletes::try_new(
            (0..2)
                .map(|index| DeferredReplaceDelete {
                    id: test_resolved_id("test", &format!("children[{index}]")),
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
        template: payload.template,
        dependency_template: payload.dependency_template,
    }))
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
    let mut parent = Resource::new("test", "parent").with_attribute("name", string("valid-parent"));
    parent.binding = Some(absolute_binding("parent"));
    let mut child = Resource::new("test", "child")
        .with_attribute("name", Value::root_resource_ref("parent", "name", vec![]));
    child.binding = Some(absolute_binding("child"));
    let resources = resolved_fixture_resources(vec![parent, child]);
    let mut plan = Plan::new();
    replacement(&mut plan, resources[0].clone(), false);
    replacement_with_delete_dependencies(
        &mut plan,
        resources[1].clone(),
        false,
        crate::resource::PersistedDestroyDependencies::from_test_dependencies([
            resources[0].resolved_id()
        ]),
    );
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
