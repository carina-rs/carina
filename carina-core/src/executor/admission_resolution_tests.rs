//! Apply must retain the authored validation scope across plan normalization.

use super::*;

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
                provider_configs: &[],
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
                provider_configs: &[],
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
