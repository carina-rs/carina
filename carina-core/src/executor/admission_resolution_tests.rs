//! Apply must retain the authored validation scope across plan normalization.

use super::*;

struct FinalInputNormalizer {
    value: Value,
    operations: Arc<Mutex<Vec<&'static str>>>,
}

impl crate::provider::ProviderNormalizer for FinalInputNormalizer {
    fn normalize_desired<'a>(
        &'a self,
        resources: &'a mut [crate::resource::ProviderResource],
    ) -> BoxFuture<'a, ProviderResult<()>> {
        Box::pin(async move {
            self.operations.lock().unwrap().push("normalize");
            for resource in resources {
                resource.set_attr("value", self.value.clone());
            }
            Ok(())
        })
    }

    fn normalize_state<'a>(
        &'a self,
        _: &'a mut crate::provider::StateTransformMap,
    ) -> BoxFuture<'a, ProviderResult<()>> {
        crate::provider::ready_noop()
    }

    fn hydrate_read_state<'a>(
        &'a self,
        _: &'a mut crate::provider::StateTransformMap,
        _: &'a crate::provider::SavedAttrs,
    ) -> BoxFuture<'a, ProviderResult<()>> {
        crate::provider::ready_noop()
    }
}

async fn assert_dispatch_rejects_normalized_value(
    nested: bool,
    replacement: bool,
    conflicting: bool,
) {
    let callbacks = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&callbacks);
    let list = PatternAttributeType::list(PatternAttributeType::string_member(
        crate::schema::StringIdentityLayer::with_validator(
            crate::schema::TypeIdentity::bare("VpcId"),
            None,
            None,
            crate::schema::validator(move |_| {
                counted.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }),
            None,
        ),
        crate::schema::StringMemberConstraints::new(
            Some(crate::schema::PatternSpec::new(
                "^vpc-[a-z]+$",
                crate::schema::PatternMatch::Full,
            )),
            None,
        ),
    ));
    let invalid = Value::Concrete(ConcreteValue::StringList(vec!["bad".into()]));
    let (declaration, normalized, slot) = if nested {
        (
            PatternAttributeType::struct_(
                "Options",
                vec![PatternStructField::required("ids", list).build()],
            ),
            Value::Concrete(ConcreteValue::Map(indexmap::IndexMap::from([(
                "ids".into(),
                invalid,
            )]))),
            "value.ids",
        )
    } else {
        (list, invalid, "value")
    };
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "",
        (if conflicting {
            PatternResourceSchema::new("test").with_conflicting_replacement()
        } else {
            PatternResourceSchema::new("test").with_coexisting_replacement()
        })
        .attribute(PatternAttributeSchema::optional("value", declaration).build())
        .validate()
        .unwrap(),
    );
    let provider = MockProvider::new();
    let resource = make_resource("example", &[]);
    provider.push_create(Ok(ok_state(&resource.id)));
    let mut plan = create_plan_from_fixture_resources(vec![resource.clone()]);
    let mut current = HashMap::new();
    if replacement {
        plan.add(Effect::Delete {
            id: ResolvedResourceId::new(resource.id.clone()),
            identifier: fixture_applied_identifier("old-instance"),
            generation: EffectGeneration::Current,
            directives: Directives::default(),
            binding: resource.binding.clone(),
            dependencies: Default::default(),
            explicit_dependencies: Default::default(),
            blocked_by_updates: Default::default(),
        });
        provider.push_delete(Ok(()));
        current.insert(
            resource.id.clone(),
            Current::from_snapshot(
                State::existing(resource.id.clone(), HashMap::new())
                    .with_identifier("old-instance"),
            ),
        );
    }
    let checked = crate::input_rules::FinalizedPlan::new(plan)
        .check_inputs(&schemas, &current)
        .unwrap();
    let observer = MockObserver::new();
    let operations = Arc::new(Mutex::new(Vec::new()));
    let normalizer = crate::provider::ConfiguredNormalizer::fixture(FinalInputNormalizer {
        value: normalized,
        operations: Arc::clone(&operations),
    });
    let result = completed_result(
        execute_plan(
            &provider,
            ExecutionInput {
                provider_context: crate::parser::ProviderContext::new(
                    crate::EnvironmentSnapshot::empty(),
                ),
                plan: &checked,
                provider_check_inputs: ProviderCheckInputs::Authored,
                compositions: &[],
                bindings: ResolvedBindings::default(),
                managed_current: current,
                data_source_values: ExecutableDataSourceValues::empty(),
                normalizer: &normalizer,
                provider_configs: &crate::test_utils::resolved_provider_configs(&[]),
                factories: &[],
                schemas: &schemas,
                parallelism: crate::executor::TEST_UNCAPPED,
                precompleted_success_count: 0,
                checkpoint_sink: ExecutionCheckpointSink::noop(),
            },
            &observer,
            uncancelled_shutdown(),
        )
        .await,
    );
    assert!(
        !operations.lock().unwrap().is_empty(),
        "normalization must run before final admission"
    );
    if replacement {
        assert!(result.successfully_deleted.is_empty());
        assert_eq!(
            result.replacement_writeback.len(),
            1,
            "retain the checked pair for recovery"
        );
        assert_eq!(
            result.replacement_writeback[0].old_instances()[0]
                .identifier
                .as_str(),
            "old-instance"
        );
    }
    assert_eq!(
        callbacks.load(Ordering::SeqCst),
        0,
        "local destination membership must reject before the validate callback"
    );
    assert!(
        provider.calls().is_empty() && provider.captured_create_resources().is_empty(),
        "invalid provider normalization output reached dispatch: {:?}",
        provider.captured_create_resources()
    );
    assert_eq!(result.failure_count(), 1, "{:?}", observer.events());
    assert_eq!(result.success_count, 0);
    let rendered = format!("{:?}", observer.events());
    assert!(rendered.contains(slot), "missing slot {slot}: {rendered}");
    assert!(
        rendered.contains("provider") && rendered.contains("normaliz"),
        "missing provider normalization origin: {rendered}"
    );
}

#[tokio::test]
async fn issue_4304_3a_dispatch_rejects_provider_normalized_string_list() {
    assert_dispatch_rejects_normalized_value(false, false, false).await;
}

#[tokio::test]
async fn issue_4304_3a_dispatch_rejects_provider_normalized_nested_struct_field() {
    assert_dispatch_rejects_normalized_value(true, false, false).await;
}

#[tokio::test]
async fn issue_4304_3a_final_admission_precedes_replacement_delete_release() {
    assert_dispatch_rejects_normalized_value(false, true, false).await;
}

fn text(value: &str) -> Value {
    Value::Concrete(ConcreteValue::String(value.into()))
}

fn normalized_tags() -> Value {
    Value::Concrete(ConcreteValue::Map(indexmap::IndexMap::from([
        ("Key".into(), text("Name")),
        ("Value".into(), text("normalized")),
    ])))
}

fn normalized_input_schema(data_source: bool) -> crate::schema::ValidatedResourceSchema {
    let schema = PatternResourceSchema::new(if data_source { "Lookup" } else { "test" })
        .attribute(PatternAttributeSchema::optional("name", PatternAttributeType::string()).build())
        .attribute(
            PatternAttributeSchema::optional("runtime", PatternAttributeType::string()).build(),
        )
        .attribute(
            PatternAttributeSchema::optional(
                "tags",
                PatternAttributeType::map(PatternAttributeType::string()),
            )
            .build(),
        )
        // This portable resource validator must see only authored inputs.
        .with_declared_validators(vec![crate::schema::ValidatorType::TagsKeyValueCheck]);
    if data_source {
        schema.as_data_source()
    } else {
        schema
    }
    .validate()
    .unwrap()
}

#[tokio::test]
async fn apply_resolution_excludes_normalization_only_resource_attributes() {
    let provider = MockProvider::new();
    let producer = make_resource("producer", &[]);
    let mut authored = make_resource("consumer", &[]);
    authored.set_attr("name", text("authored"));
    authored.set_attr(
        "runtime",
        Value::root_resource_ref("producer", "value", vec![]),
    );
    authored
        .dependencies
        .value_bindings
        .insert(absolute_binding("producer"));
    let mut planned = authored.clone();
    planned.set_attr("tags", normalized_tags());

    let mut schemas = SchemaRegistry::new();
    schemas.insert("", normalized_input_schema(false));
    // The authored input passes the same resource validator at plan time.
    let attributes = authored.resolved_attributes();
    schemas
        .get("", "test", crate::schema::SchemaKind::Resource)
        .unwrap()
        .validate(&attributes)
        .unwrap();

    provider.push_create(Ok(State::existing(
        producer.id.clone(),
        crate::test_support::snapshot_attributes(HashMap::from([(
            "value".into(),
            text("resolved"),
        )])),
    )
    .with_identifier("producer-id")));
    provider.push_create(Ok(ok_state(&authored.id)));
    let plan = create_plan_from_fixture_resources(vec![producer.clone(), planned]);
    let origins = [producer, authored]
        .into_iter()
        .map(|resource| {
            (
                ResolvedResourceId::new(resource.id.clone()),
                UnresolvedResource::from_pre_resolve(resource),
            )
        })
        .collect();
    let checked = checked_fixture_plan(&plan);
    let observer = MockObserver::new();
    let result = completed_result(
        execute_plan(
            &provider,
            ExecutionInput {
                provider_context: crate::parser::ProviderContext::new(
                    crate::EnvironmentSnapshot::empty(),
                ),
                plan: &checked,
                provider_check_inputs: ProviderCheckInputs::PlanNormalized {
                    resource_origins: &origins,
                    data_source_origins: &[],
                },
                compositions: &[],
                bindings: ResolvedBindings::default(),
                managed_current: HashMap::new(),
                data_source_values: ExecutableDataSourceValues::empty(),
                normalizer: &NoopNormalizer,
                provider_configs: &crate::test_utils::resolved_provider_configs(&[]),
                factories: &[],
                schemas: &schemas,
                parallelism: crate::executor::TEST_UNCAPPED,
                precompleted_success_count: 0,
                checkpoint_sink: ExecutionCheckpointSink::noop(),
            },
            &observer,
            uncancelled_shutdown(),
        )
        .await,
    );
    assert_eq!(result.failure_count(), 0, "{:?}", observer.events());
    assert_eq!(result.success_count, 2);
    let requests = provider.captured_create_resources();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1].get_attr("tags"), Some(&normalized_tags()));
    assert_eq!(requests[1].get_attr("runtime"), Some(&text("resolved")));
}

#[tokio::test]
async fn apply_resolution_excludes_normalization_only_data_source_attributes() {
    let provider = MockProvider::new();
    let producer = make_resource("producer", &[]);
    let mut authored = DataSource::with_provider("test", "Lookup", "lookup", None);
    authored.binding = Some(absolute_binding("lookup"));
    authored.set_attr("name", text("authored"));
    authored.set_attr(
        "runtime",
        Value::root_resource_ref("producer", "value", vec![]),
    );
    let mut planned = authored.clone();
    planned.set_attr("tags", normalized_tags());
    let mut schemas = SchemaRegistry::new();
    schemas.insert("", normalized_input_schema(false));
    schemas.insert("test", normalized_input_schema(true));
    provider.push_create(Ok(State::existing(
        producer.id.clone(),
        crate::test_support::snapshot_attributes(HashMap::from([(
            "value".into(),
            text("resolved"),
        )])),
    )
    .with_identifier("producer-id")));
    provider.push_read(Ok(
        State::existing(authored.id.clone(), HashMap::new()).with_identifier("lookup-id")
    ));
    let (mut managed, mut lookups) =
        resolved_fixture_graph(std::slice::from_ref(&producer), &[planned]);
    let mut plan = Plan::new();
    plan.add(Effect::Create(managed.remove(0)));
    plan.add(Effect::Read {
        resource: lookups.remove(0),
    });
    let values = executable_deferred_data_source_values(std::slice::from_ref(&authored));
    let origins = HashMap::from([(
        ResolvedResourceId::new(producer.id.clone()),
        UnresolvedResource::from_pre_resolve(producer),
    )]);
    let data_source_origins = [authored.into_value_snapshot()];
    let checked = checked_fixture_plan(&plan);
    let observer = MockObserver::new();
    let result = completed_result(
        execute_plan(
            &provider,
            ExecutionInput {
                provider_context: crate::parser::ProviderContext::new(
                    crate::EnvironmentSnapshot::empty(),
                ),
                plan: &checked,
                provider_check_inputs: ProviderCheckInputs::PlanNormalized {
                    resource_origins: &origins,
                    data_source_origins: &data_source_origins,
                },
                compositions: &[],
                bindings: ResolvedBindings::default(),
                managed_current: HashMap::new(),
                data_source_values: values,
                normalizer: &NoopNormalizer,
                provider_configs: &crate::test_utils::resolved_provider_configs(&[]),
                factories: &[],
                schemas: &schemas,
                parallelism: crate::executor::TEST_UNCAPPED,
                precompleted_success_count: 0,
                checkpoint_sink: ExecutionCheckpointSink::noop(),
            },
            &observer,
            uncancelled_shutdown(),
        )
        .await,
    );
    assert_eq!(result.failure_count(), 0, "{:?}", observer.events());
    assert_eq!(result.success_count, 2);
    let requests = provider.captured_data_source_reads();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].get_attr("tags"), Some(&normalized_tags()));
    assert_eq!(requests[0].get_attr("runtime"), Some(&text("resolved")));
}

#[tokio::test]
async fn issue_4304_3a_review1_destroy_first_final_admission_holds_delete() {
    assert_dispatch_rejects_normalized_value(false, true, true).await;
}

async fn review1_deferred_final_admission(replace: bool) {
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "",
        PatternResourceSchema::new("test")
            .with_conflicting_replacement()
            .attribute(
                PatternAttributeSchema::optional("value", PatternAttributeType::bool()).build(),
            )
            .validate()
            .unwrap(),
    );
    let template = crate::parser::DeferredForExpression::new(
        Some("main.crn".into()),
        1,
        "for item in ['one', 'two']".into(),
        crate::parser::DeferredForTarget::bound(absolute_binding("children")).unwrap(),
        Value::Concrete(ConcreteValue::List(vec![text("one"), text("two")])),
        crate::parser::ForBinding::Simple("item".into()),
        crate::resource::Resource::<ModuleSource, LoopSlot>::new("test", "children"),
    );
    let compiled = crate::test_support::compiled_loop(Box::new(template));
    let old = (0..2)
        .map(|i| DeferredReplaceDelete {
            id: ResolvedResourceId::new(ResourceId::with_identity(
                "test",
                ResourceIdentity::new(format!("children[{i}]")),
            )),
            identifier: fixture_applied_identifier(format!("old-{i}")),
            generation: EffectGeneration::Current,
            previous_attributes: HashMap::new(),
            directives: Default::default(),
            binding: Some(absolute_binding(&format!("children[{i}]"))),
            dependencies: Default::default(),
            explicit_dependencies: Default::default(),
            blocked_by_updates: Default::default(),
        })
        .collect::<Vec<_>>();
    let current = old
        .iter()
        .map(|old| {
            (
                old.id.as_inner().clone(),
                Current::from_snapshot(
                    State::existing(old.id.as_inner().clone(), HashMap::new())
                        .with_identifier(old.identifier.as_str()),
                ),
            )
        })
        .collect::<HashMap<_, _>>();
    let mut plan = Plan::new();
    if replace {
        plan.add(Effect::DeferredReplace(Box::new(
            crate::effect::DeferredReplacePayload::new(
                NonEmptyDeletes::try_new(old.clone()).unwrap(),
                compiled.template,
                compiled.dependency_template,
            ),
        )));
    } else {
        for delete in &old {
            plan.add(delete.to_delete_effect());
        }
        plan.add(Effect::DeferredCreate(compiled));
    }
    let checked = crate::input_rules::FinalizedPlan::new(plan)
        .check_inputs(&schemas, &current)
        .unwrap();
    assert!(
        checked.pending_inputs().is_empty(),
        "final admission must protect replacements without authored input rules"
    );
    let provider = MockProvider::new();
    for delete in &old {
        provider.push_delete(Ok(()));
        provider.push_create(Ok(ok_state(delete.id.as_inner())));
    }
    let operations = Arc::new(Mutex::new(Vec::new()));
    let normalizer = crate::provider::ConfiguredNormalizer::fixture(FinalInputNormalizer {
        value: if replace {
            text("invalid normalized Bool")
        } else {
            Value::Concrete(ConcreteValue::Bool(true))
        },
        operations: Arc::clone(&operations),
    });
    let observer = MockObserver::new();
    let result = completed_result(
        execute_plan(
            &provider,
            ExecutionInput {
                provider_context: crate::parser::ProviderContext::new(
                    crate::EnvironmentSnapshot::empty(),
                ),
                plan: &checked,
                provider_check_inputs: ProviderCheckInputs::Authored,
                compositions: &[],
                bindings: ResolvedBindings::default(),
                managed_current: current.clone(),
                data_source_values: ExecutableDataSourceValues::empty(),
                normalizer: &normalizer,
                provider_configs: &crate::test_utils::resolved_provider_configs(&[]),
                factories: &[],
                schemas: &schemas,
                parallelism: NonZeroUsize::new(1).unwrap(),
                precompleted_success_count: 0,
                checkpoint_sink: ExecutionCheckpointSink::noop(),
            },
            &observer,
            uncancelled_shutdown(),
        )
        .await,
    );
    assert!(!operations.lock().unwrap().is_empty());
    if replace {
        assert_eq!(result.failure_count(), 1, "{:?}", observer.events());
        assert!(provider.calls().is_empty(), "{:?}", provider.calls());
        assert!(result.successfully_deleted.is_empty());
        assert_eq!(result.managed_current, current);
        let message = format!("{:?}", observer.events());
        assert!(
            message.contains("value") && message.contains("normaliz"),
            "{message}"
        );
    } else {
        assert_eq!(result.failure_count(), 0, "{:?}", observer.events());
        let calls = provider.calls();
        assert_eq!(calls.len(), 4, "{calls:?}");
        for delete in &old {
            let id = delete.id.to_string();
            let teardown = calls
                .iter()
                .position(|(op, row)| op == "delete" && row == &id)
                .unwrap();
            let create = calls
                .iter()
                .position(|(op, row)| op == "create" && row == &id)
                .unwrap();
            assert!(
                teardown < create,
                "destroy-first order must match main: {calls:?}"
            );
        }
        assert_eq!(provider.captured_create_resources().len(), 2);
    }
}

#[tokio::test]
async fn issue_4304_3a_review1_deferred_replace_final_admission_holds_deletes() {
    review1_deferred_final_admission(true).await;
}

#[tokio::test]
async fn issue_4304_3a_review1_deferred_create_delete_candidates_preserve_success_order() {
    review1_deferred_final_admission(false).await;
}
