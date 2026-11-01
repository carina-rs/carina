use super::*;
use crate::effect::PlanOp;
use crate::provider::*;
use crate::resource::{ApplyTimeUnknown, ConcreteValue, DeferredValue, Resource, State};
use crate::schema::{AttributeSchema, ValidatedResourceSchema};
use carina_provider_protocol::types as wire;
use std::sync::{Arc, Mutex};

type TableResult = Result<wire::ReferenceOutcome, wire::ReferenceError>;

struct TableProvider {
    table: HashMap<String, TableResult>,
    calls: Arc<Mutex<Vec<Vec<ReferenceRequest>>>>,
    barrier: Option<Arc<tokio::sync::Barrier>>,
    result_count: Option<usize>,
    fail_batch: bool,
    updates: Arc<Mutex<Vec<UpdateRequest>>>,
}
impl Provider for TableProvider {
    fn name(&self) -> &str {
        "test"
    }
    fn resolve_references<'a>(
        &'a self,
        requests: &'a [ReferenceRequest],
    ) -> BoxFuture<'a, ProviderResult<Vec<Result<ReferenceOutcome, ReferenceError>>>> {
        self.calls.lock().unwrap().push(requests.to_vec());
        Box::pin(async move {
            if let Some(barrier) = &self.barrier {
                barrier.wait().await;
            }
            if self.fail_batch {
                return Err(ProviderError::from_wire(*error()));
            }
            Ok(requests
                .iter()
                .cycle()
                .take(self.result_count.unwrap_or(requests.len()))
                .map(|request| {
                    match self.table.get(&request.value).cloned().unwrap_or_else(|| {
                        Ok(wire::ReferenceOutcome::Resolved(request.value.clone()))
                    }) {
                        Ok(outcome) => ReferenceOutcome::from_guest(outcome),
                        Err(wire::ReferenceError::AccessDenied(error)) => Err(
                            ReferenceError::AccessDenied(ProviderError::from_wire(*error).into()),
                        ),
                        Err(wire::ReferenceError::Transient(error)) => Err(
                            ReferenceError::Transient(ProviderError::from_wire(*error).into()),
                        ),
                        Err(wire::ReferenceError::InvalidReference(error)) => {
                            Err(ReferenceError::InvalidReference(
                                ProviderError::from_wire(*error).into(),
                            ))
                        }
                    }
                })
                .collect())
        })
    }
    fn read(
        &self,
        _: &ResourceId,
        _: &AppliedIdentifier,
        _: ReadRequest,
    ) -> BoxFuture<'_, ProviderResult<ReadOutcome>> {
        unimplemented!()
    }
    fn read_data_source(
        &self,
        _: &ProviderReadyDataSource,
    ) -> BoxFuture<'_, ProviderResult<State>> {
        unimplemented!()
    }
    fn create(
        &self,
        _: &ResourceId,
        _: CreateRequest,
        _: CreateAttempt,
    ) -> BoxFuture<'_, Result<CreateOutcome, CreateFailure>> {
        unimplemented!()
    }
    fn resume_create(
        &self,
        _: ResumeCreateRequest,
    ) -> BoxFuture<'_, Result<ResumeOutcome, ResumeFailure>> {
        unimplemented!()
    }
    fn update(
        &self,
        id: &ResourceId,
        identifier: &AppliedIdentifier,
        request: UpdateRequest,
    ) -> BoxFuture<'_, ProviderResult<UpdateOutcome>> {
        let mut attributes = request.from().attributes.clone();
        for op in &request.patch().ops {
            match op.sent_value() {
                Some(value) => {
                    attributes.insert(op.key().into(), value.clone());
                }
                None => {
                    attributes.remove(op.key());
                }
            }
        }
        let state = AppliedState::from_identifier(id.clone(), identifier.clone(), attributes);
        self.updates.lock().unwrap().push(request);
        Box::pin(async move { Ok(UpdateOutcome::success(state)) })
    }
    fn delete(
        &self,
        _: &ResourceId,
        _: &AppliedIdentifier,
        _: DeleteRequest,
    ) -> BoxFuture<'_, ProviderResult<()>> {
        unimplemented!()
    }
    fn required_permissions(&self, _: &ResourceId, _: PlanOp) -> ProviderResult<Vec<String>> {
        Ok(Vec::new())
    }
}

fn identity() -> TypeIdentity {
    TypeIdentity::new(Some("test"), ["keys", "Key"], "Reference")
}
fn string(s: &str) -> Value {
    Value::Concrete(ConcreteValue::String(s.into()))
}
fn error() -> Box<wire::ProviderError> {
    Box::new(wire::ProviderError {
        kind: wire::ProviderErrorKind::ApiError,
        message: "service diagnostic".into(),
        resource_id: None,
        cause: Some("transport diagnostic".into()),
        provider_name: Some("test".into()),
        operation: Some("ResolveKey".into()),
        status: Some(403),
        code: Some("DeniedCode".into()),
        request_id: Some("request-123".into()),
    })
}
fn schema(create_only: bool, write_only: bool) -> ValidatedResourceSchema {
    let mut attribute = AttributeSchema::optional(
        "key",
        crate::schema::AttributeType::reference(identity(), None, None),
    );
    if create_only {
        let mut attribute = attribute.create_only();
        if write_only {
            attribute = attribute.write_only();
        }
        return crate::schema::ResourceSchema::new("keys.Resource")
            .attribute(attribute.build())
            .validate()
            .unwrap();
    }
    if write_only {
        attribute = attribute.write_only();
    }
    crate::schema::ResourceSchema::new("keys.Resource")
        .attribute(attribute.build())
        .validate()
        .unwrap()
}
struct Fixture {
    desired: Vec<ResolvedResource>,
    current: HashMap<ResourceId, PlanInputCurrent>,
    registry: SchemaRegistry,
    saved: LiftedSavedAttrs,
    router: ProviderRouter,
    calls: Arc<Mutex<Vec<Vec<ReferenceRequest>>>>,
    updates: Arc<Mutex<Vec<UpdateRequest>>>,
}
impl Fixture {
    fn new(
        desired: Value,
        current: Value,
        table: HashMap<String, TableResult>,
        create_only: bool,
    ) -> Self {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let updates = Arc::new(Mutex::new(Vec::new()));
        let mut router = ProviderRouter::new();
        router.add_provider(
            "test".into(),
            InitializedProvider::unavailable(Box::new(TableProvider {
                table,
                calls: calls.clone(),
                barrier: None,
                result_count: None,
                fail_batch: false,
                updates: updates.clone(),
            })),
        );
        let mut registry = SchemaRegistry::new();
        registry.insert("test", schema(create_only, false));
        let resource = Resource::with_provider("test", "keys.Resource", "example", None)
            .with_root_binding("example")
            .with_attribute("key", desired);
        let state = State {
            id: resource.id.clone(),
            identifier: Some("resource-123".into()),
            attributes: HashMap::from([("key".into(), current)]),
            exists: true,
            dependency_bindings: BTreeSet::new(),
            partial_read: None,
        };
        let current = crate::resource::into_plan_input_current_map(
            HashMap::from([(resource.id.clone(), Current::from_state(state))]),
            &registry,
            std::slice::from_ref(&resource),
        );
        let desired =
            ResolvedResource::collect_resolved_with_schemas([resource], &registry).unwrap();
        let saved = RawSavedAttrs::default().lift(&registry);
        Self {
            desired,
            current,
            registry,
            saved,
            router,
            calls,
            updates,
        }
    }
    fn reference_values(&self) -> Result<Vec<Occurrence>, Box<ReferencePlanError>> {
        collect_inputs(&self.desired, &self.current, &self.registry, &self.saved)
    }
    async fn prepare(&self) -> Result<ResolvedReferences, Box<ReferencePlanError>> {
        ReferencePreparation::default()
            .prepare(
                &self.desired,
                &self.current,
                &self.registry,
                &self.saved,
                &self.router,
            )
            .await
    }
    fn plan(
        &self,
        references: &ResolvedReferences,
    ) -> crate::differ::PlanComparisonResult<crate::plan::Plan> {
        self.plan_with_explicit(references, &HashMap::new())
    }
    fn plan_with_explicit(
        &self,
        references: &ResolvedReferences,
        explicit: &HashMap<ResourceId, crate::explicit::ExplicitFields>,
    ) -> crate::differ::PlanComparisonResult<crate::plan::Plan> {
        crate::differ::create_plan(
            &self.desired,
            &[],
            &self.router,
            crate::differ::PlanPriorStates::new(&self.current, &crate::differ::Orphans::new()),
            &HashMap::new(),
            &self.registry,
            &self.saved,
            explicit,
            &[],
            references,
        )
    }
}
fn resolved(value: &str) -> TableResult {
    Ok(wire::ReferenceOutcome::Resolved(value.into()))
}

fn round5_list_fixture(ordered: bool, create_only: bool) -> Fixture {
    use crate::schema::{AttributeType, StructField};
    let mut fixture = Fixture::new(
        string("unused"),
        string("unused"),
        HashMap::from([("alias/b".into(), resolved("arn:B"))]),
        false,
    );
    let kms = StructField::optional("kms", AttributeType::reference(identity(), None, None));
    let target = StructField::optional("target", AttributeType::string());
    let element = AttributeType::struct_(
        "Entry",
        vec![
            if create_only {
                kms.create_only().build()
            } else {
                kms.build()
            },
            StructField::optional("x", AttributeType::string()).build(),
            if create_only {
                target.create_only().build()
            } else {
                target.build()
            },
        ],
    );
    let attribute = AttributeSchema::optional(
        "key",
        if ordered {
            AttributeType::list(element)
        } else {
            AttributeType::unordered_list(element)
        },
    );
    let attribute = attribute.build();
    fixture.registry.insert(
        "test",
        crate::schema::ResourceSchema::new("keys.Resource")
            .attribute(attribute)
            .with_coexisting_replacement()
            .validate()
            .unwrap(),
    );
    fixture
}

fn round5_entry(kms: &str, x: &str, target: Value) -> Value {
    Value::Concrete(ConcreteValue::Map(indexmap::IndexMap::from([
        ("kms".into(), string(kms)),
        ("x".into(), string(x)),
        ("target".into(), target),
    ])))
}

#[tokio::test]
async fn round5_ordered_list_detail_rows_record_the_pairs_the_renderer_compares() {
    let mut fixture = round5_list_fixture(true, false);
    let desired = Value::Concrete(ConcreteValue::List(vec![
        round5_entry("alias/a", "1", string("same")),
        round5_entry("alias/b", "2", string("same")),
    ]));
    let current = Value::Concrete(ConcreteValue::List(vec![
        round5_entry("alias/a", "9", string("same")),
        round5_entry("arn:B", "2", string("same")),
    ]));
    reset_values(&mut fixture, desired, current.clone(), current);
    fixture.saved = RawSavedAttrs::default().lift(&fixture.registry);
    let references = fixture.prepare().await.unwrap();
    let plan = fixture.plan(&references).unwrap();
    assert!(matches!(
        &plan.effects()[0],
        crate::effect::Effect::Update { .. }
    ));
    let rows = crate::detail_rows::build_detail_rows_by_instance(
        &plan.effects()[0],
        Some(&fixture.registry),
        crate::detail_rows::DetailLevel::Full,
        &crate::detail_rows::HistoricalDisplayAttributes::for_plan(&plan),
        None,
        &fixture.router,
        crate::detail_rows::DetailComparisons::Planning(&references),
    )
    .expect("rendering must participate in reference recording");
    assert!(!rows.is_empty());
    let values = fixture
        .calls
        .lock()
        .unwrap()
        .iter()
        .flatten()
        .map(|request| request.value.clone())
        .collect::<BTreeSet<_>>();
    assert!(values.contains("alias/b") && values.contains("arn:B"));
    assert!(values.is_subset(&BTreeSet::from([
        "alias/a".into(),
        "alias/b".into(),
        "arn:B".into()
    ])));
}

#[tokio::test]
async fn round5_cascade_unordered_list_with_resource_ref_records_its_own_pairs() {
    use crate::differ::{CascadeDependents, DeferredCreateTargets, DeferredForExpansionChoice};
    use crate::schema::AttributeType;
    let mut fixture = round5_list_fixture(false, true);
    fixture.registry.insert(
        "test",
        crate::schema::ResourceSchema::new("keys.Producer")
            .attribute(
                AttributeSchema::optional("name", AttributeType::string())
                    .create_only()
                    .build(),
            )
            .with_coexisting_replacement()
            .validate()
            .unwrap(),
    );
    let list = |kms: &str, target: Value| {
        Value::Concrete(ConcreteValue::List(vec![
            round5_entry("alias/a", "1", target.clone()),
            round5_entry(kms, "2", target),
        ]))
    };
    reset_values(
        &mut fixture,
        list("alias/b", string("old-id")),
        list("arn:B", string("old-id")),
        list("arn:B", string("old-id")),
    );
    fixture.saved = RawSavedAttrs::default().lift(&fixture.registry);
    let producer = Resource::with_provider("test", "keys.Producer", "producer", None)
        .with_root_binding("producer")
        .with_attribute("name", string("new"));
    let old = State::existing(
        producer.id.clone(),
        HashMap::from([("name".into(), string("old"))]),
    )
    .with_identifier("old-id");
    fixture
        .current
        .extend(crate::resource::into_plan_input_current_map(
            HashMap::from([(producer.id.clone(), Current::from_state(old))]),
            &fixture.registry,
            std::slice::from_ref(&producer),
        ));
    let mut unresolved = fixture.desired[0].as_resource().clone();
    unresolved.attributes.insert(
        "key".into(),
        list(
            "alias/b",
            Value::root_resource_ref("producer", "id", Vec::new()),
        ),
    );
    let unresolved = ResolvedResource::collect_resolved_with_schemas(
        [producer.clone(), unresolved],
        &fixture.registry,
    )
    .unwrap();
    fixture.desired.insert(
        0,
        ResolvedResource::resolve_with_schemas(producer, &fixture.registry).unwrap(),
    );
    let desired =
        crate::resource::plan_input_desired_resources(&fixture.desired, &fixture.registry);
    let dependents = CascadeDependents::new(
        &unresolved,
        DeferredCreateTargets::partition_for_expansion(
            crate::parser::ExpandedFile::<()>::default().into_deferred_for_expressions(),
            |_| DeferredForExpansionChoice::Retain,
        )
        .1,
    );
    let mut preparation = ReferencePreparation::default();
    let prepared = preparation
        .plan_with_cascades(
            &desired,
            &dependents,
            &[],
            &fixture.router,
            crate::differ::PlanPriorStates::new(&fixture.current, &crate::differ::Orphans::new()),
            &HashMap::new(),
            &fixture.registry,
            &fixture.saved,
            &HashMap::new(),
            &[],
            &HashSet::new(),
        )
        .await
        .expect("cascade comparison must participate in reference recording");
    let plan = prepared.value.finish(|_| {});
    preparation
        .display(
            plan.clone(),
            &prepared.references,
            crate::detail_rows::HistoricalDisplayAttributes::for_planning(&plan, &fixture.saved),
            crate::detail_rows::DetailLevel::Full,
            &fixture.registry,
            &HashMap::new(),
            &fixture.router,
        )
        .await
        .unwrap();
    assert!(
        fixture
            .calls
            .lock()
            .unwrap()
            .iter()
            .flatten()
            .any(|request| request.value == "alias/a")
    );
    assert!(!plan.effects().is_empty());
}

#[tokio::test]
async fn successive_preparations_and_detail_rendering_share_a_router_without_global_evidence() {
    let fixture = Fixture::new(
        string("alias/live"),
        string("key/a"),
        HashMap::from([("alias/live".into(), resolved("key/b"))]),
        false,
    );
    for _ in 0..2 {
        let references = fixture.prepare().await.unwrap();
        let plan = fixture.plan(&references).unwrap();
        let effect = plan
            .effects()
            .first()
            .expect("reference repoint produces an Update");
        let historical = crate::detail_rows::HistoricalDisplayAttributes::for_plan(&plan);
        let rows = crate::detail_rows::build_detail_rows_by_instance(
            effect,
            Some(&fixture.registry),
            crate::detail_rows::DetailLevel::Full,
            &historical,
            None,
            &fixture.router,
            crate::detail_rows::DetailComparisons::Planning(&references),
        )
        .unwrap();
        assert!(format!("{rows:?}").contains("alias/live"));
    }
    assert_eq!(
        fixture.calls.lock().unwrap().len(),
        2,
        "new preparations resolve afresh"
    );
}

#[tokio::test]
async fn byte_equal_references_do_not_resolve_even_if_alias_was_repointed() {
    let fixture = Fixture::new(
        string("alias/live"),
        string("alias/live"),
        HashMap::from([(
            "alias/live".into(),
            Err(wire::ReferenceError::AccessDenied(error())),
        )]),
        true,
    );
    let witness = fixture.prepare().await.unwrap();
    assert!(fixture.plan(&witness).unwrap().effects().is_empty());
    assert!(fixture.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn round8_enumerates_other_resources_without_judging_unused_outcomes() {
    for outcome in [
        Err(wire::ReferenceError::AccessDenied(error())),
        Err(wire::ReferenceError::Transient(error())),
        Ok(wire::ReferenceOutcome::NotFound(error())),
    ] {
        for repointed in [false, true] {
            let mut fixture = Fixture::new(
                string("alias/x"),
                string("key/a"),
                HashMap::from([
                    (
                        "alias/x".into(),
                        resolved(if repointed { "key/b" } else { "key/a" }),
                    ),
                    ("alias/y".into(), outcome.clone()),
                ]),
                false,
            );
            let other = Resource::with_provider("test", "keys.Resource", "other", None)
                .with_root_binding("other")
                .with_attribute("key", string("alias/y"));
            fixture
                .current
                .extend(crate::resource::into_plan_input_current_map(
                    HashMap::from([(
                        other.id.clone(),
                        Current::from_state(
                            State::existing(
                                other.id.clone(),
                                HashMap::from([("key".into(), string("alias/y"))]),
                            )
                            .with_identifier("other-id"),
                        ),
                    )]),
                    &fixture.registry,
                    std::slice::from_ref(&other),
                ));
            fixture.desired.extend(
                ResolvedResource::collect_resolved_with_schemas([other], &fixture.registry)
                    .unwrap(),
            );
            let references = fixture.prepare().await.unwrap();
            let plan = fixture.plan(&references).unwrap();
            assert_eq!(plan.effects().len(), usize::from(repointed));
            assert_eq!(references.dangling_paths(&fixture.desired[1].id).count(), 0);
            let calls = fixture.calls.lock().unwrap();
            assert_eq!(calls.len(), 1);
            assert_eq!(
                calls[0]
                    .iter()
                    .map(|request| request.value.as_str())
                    .collect::<BTreeSet<_>>(),
                BTreeSet::from(["alias/x", "alias/y", "key/a"]),
                "the miss on X must prepare byte-equal Y too"
            );
        }
    }
}

#[tokio::test]
async fn round8_unordered_pairing_changes_control_flow_in_exactly_one_replay() {
    let mut fixture = round5_list_fixture(false, false);
    fixture.router.add_provider(
        "test".into(),
        InitializedProvider::unavailable(Box::new(TableProvider {
            table: HashMap::from([
                ("alias/a".into(), resolved("arn:A")),
                ("alias/b".into(), resolved("arn:B")),
            ]),
            calls: fixture.calls.clone(),
            barrier: None,
            result_count: None,
            fail_batch: false,
            updates: fixture.updates.clone(),
        })),
    );
    let list = |entries: &[(&str, &str)]| {
        Value::Concrete(ConcreteValue::List(
            entries
                .iter()
                .map(|(key, x)| round5_entry(key, x, string("same")))
                .collect(),
        ))
    };
    let current = list(&[("arn:B", "2"), ("arn:A", "1")]);
    reset_values(
        &mut fixture,
        list(&[("alias/a", "1"), ("alias/b", "2")]),
        current.clone(),
        current,
    );
    fixture.saved = RawSavedAttrs::default().lift(&fixture.registry);
    let mut passes = 0;
    let prepared = ReferencePreparation::default()
        .run(
            &fixture.router,
            || fixture.reference_values(),
            |session| {
                passes += 1;
                session.result(fixture.plan(session.references))
            },
        )
        .await
        .unwrap();
    assert!(prepared.value.effects().is_empty());
    assert_eq!(
        passes, 2,
        "changed list pairing must use the single prepared replay"
    );
    assert_eq!(fixture.calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn round8_a_miss_after_replay_is_an_internal_error_with_its_operand_path() {
    let fixture = Fixture::new(
        string("alias/first"),
        string("key/a"),
        HashMap::new(),
        false,
    );
    let mut passes = 0;
    let error = ReferencePreparation::default()
        .run(
            &fixture.router,
            || fixture.reference_values(),
            |session| {
                passes += 1;
                let (raw, path) = if passes == 1 {
                    ("alias/first", "key")
                } else {
                    ("alias/outside-inputs", "key.late[1]")
                };
                session.result(session.references.equal(
                    &fixture.desired[0].id,
                    &identity(),
                    &raw.into(),
                    &"key/a".into(),
                    &[
                        ReferenceOperand {
                            path: path.into(),
                            side: ReferenceSide::Desired,
                        },
                        ReferenceOperand {
                            path: "key".into(),
                            side: ReferenceSide::Current,
                        },
                    ],
                ))
            },
        )
        .await
        .expect_err("an operand outside the invocation cannot trigger a third computation");
    assert!(matches!(
        error.source,
        ReferenceFailure::UnpreparedAfterReplay
    ));
    assert_eq!(error.resource, fixture.desired[0].id);
    assert_eq!(error.path, "key.late[1]");
    assert_eq!(error.raw.expose_plaintext(), "alias/outside-inputs");
    assert_eq!(passes, 2);
    assert_eq!(fixture.calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn round8_clean_first_pass_never_enumerates_or_replays() {
    let fixture = Fixture::new(
        string("alias/equal"),
        string("alias/equal"),
        HashMap::new(),
        false,
    );
    let mut passes = 0;
    let prepared = ReferencePreparation::default()
        .run(
            &fixture.router,
            || panic!("a clean first pass needs no enumeration"),
            |session| {
                passes += 1;
                session.result(fixture.plan(session.references))
            },
        )
        .await
        .unwrap();
    assert!(prepared.value.effects().is_empty());
    assert_eq!(passes, 1);
    assert!(fixture.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn round9_scalar_to_list_union_enumerates_planning_and_display_operands() {
    use crate::schema::AttributeType;
    let mut fixture = Fixture::new(
        string("unused"),
        string("unused"),
        HashMap::from([
            ("alias/desired".into(), resolved("key/b")),
            ("alias/saved".into(), resolved("key/a")),
        ]),
        false,
    );
    fixture.registry.insert(
        "test",
        crate::schema::ResourceSchema::new("keys.Resource")
            .attribute(
                AttributeSchema::optional(
                    "key",
                    AttributeType::union(vec![
                        AttributeType::union(vec![
                            AttributeType::string(),
                            AttributeType::list(AttributeType::string()),
                        ]),
                        AttributeType::list(AttributeType::reference(identity(), None, None)),
                    ]),
                )
                .build(),
            )
            .validate()
            .unwrap(),
    );
    reset_values(
        &mut fixture,
        string("alias/desired"),
        string("key/a"),
        string("alias/saved"),
    );
    let id = &fixture.desired[0].id;
    assert_eq!(
        fixture.desired[0].attributes["key"],
        string("alias/desired")
    );
    let saved_before = fixture.saved.get(id).unwrap().clone();
    assert!(matches!(
        fixture.current[id].comparison_attributes().get("key"),
        Some(Value::Concrete(ConcreteValue::StringList(_)))
    ));

    let references = fixture
        .prepare()
        .await
        .expect("the canonical list operands must all be prepared in one replay");
    let plan = fixture.plan(&references).unwrap();
    assert_eq!(plan.effects().len(), 1);
    let check_batch = || {
        let calls = fixture.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0]
                .iter()
                .map(|request| request.value.as_str())
                .collect::<BTreeSet<_>>(),
            BTreeSet::from(["alias/desired", "alias/saved", "key/a"])
        );
    };
    check_batch();
    fixture.calls.lock().unwrap().clear();
    ReferencePreparation::default()
        .display(
            plan.clone(),
            &prepare_empty_for_tests(),
            crate::detail_rows::HistoricalDisplayAttributes::for_planning(&plan, &fixture.saved),
            crate::detail_rows::DetailLevel::Full,
            &fixture.registry,
            &HashMap::new(),
            &fixture.router,
        )
        .await
        .expect("fresh display preparation must enumerate canonical historical operands too");
    check_batch();
    assert_eq!(
        fixture.desired[0].attributes["key"],
        string("alias/desired")
    );
    assert_eq!(fixture.saved.get(id).unwrap(), &saved_before);
}

#[tokio::test]
async fn round9_used_whole_batch_error_is_typed_through_the_planner_and_not_retried() {
    let mut fixture = Fixture::new(
        string("alias/desired"),
        string("key/a"),
        HashMap::new(),
        false,
    );
    fixture.router.add_provider(
        "test".into(),
        InitializedProvider::unavailable(Box::new(TableProvider {
            table: HashMap::new(),
            calls: fixture.calls.clone(),
            barrier: None,
            result_count: None,
            fail_batch: true,
            updates: fixture.updates.clone(),
        })),
    );
    let mut preparation = ReferencePreparation::default();
    for _ in 0..2 {
        let error = preparation
            .plan(
                &fixture.desired,
                &[],
                &fixture.router,
                crate::differ::PlanPriorStates::new(
                    &fixture.current,
                    &crate::differ::Orphans::new(),
                ),
                &HashMap::new(),
                &fixture.registry,
                &fixture.saved,
                &HashMap::new(),
                &[],
            )
            .await
            .expect_err("a used batch failure must prevent a plan from escaping");
        let crate::differ::PlanComparisonError::Reference(error) = error else {
            panic!("expected typed reference plan error, got {error:?}");
        };
        assert!(matches!(error.source, ReferenceFailure::Call(_)));
        assert_eq!(error.resource, fixture.desired[0].id);
        assert_eq!(error.instance, ProviderInstanceKey::new("test", None));
        assert_eq!(error.path, "key");
        assert_eq!(error.raw.expose_plaintext(), "key/a");
        let mut source: Option<&(dyn std::error::Error + 'static)> = Some(error.as_ref());
        let mut chain = String::new();
        while let Some(error) = source {
            chain.push_str(&error.to_string());
            source = error.source();
        }
        for required in [
            "example",
            "key/a",
            "DeniedCode",
            "request-123",
            "transport diagnostic",
        ] {
            assert!(chain.contains(required), "missing {required}: {chain}");
        }
        assert_eq!(fixture.calls.lock().unwrap().len(), 1);
    }
}

#[tokio::test]
async fn round8_whole_batch_errors_are_cached_for_each_value_and_judged_on_use() {
    let mut fixture = Fixture::new(
        string("alias/live"),
        string("key/a"),
        HashMap::from([("alias/live".into(), resolved("key/a"))]),
        false,
    );
    let mut other = Resource::with_provider("test", "keys.Resource", "other", None)
        .with_root_binding("other")
        .with_attribute("key", string("alias/untouched"));
    other.id.provider_instance = Some("other".into());
    let id = other.id.clone();
    fixture
        .current
        .extend(crate::resource::into_plan_input_current_map(
            HashMap::from([(
                id.clone(),
                Current::from_state(
                    State::existing(
                        id.clone(),
                        HashMap::from([("key".into(), string("alias/untouched"))]),
                    )
                    .with_identifier("other-id"),
                ),
            )]),
            &fixture.registry,
            std::slice::from_ref(&other),
        ));
    fixture.saved = RawSavedAttrs::from_decoded_values_for_test(HashMap::from([(
        id.clone(),
        HashMap::from([("key".into(), string("alias/unused-saved"))]),
    )]))
    .lift(&fixture.registry);
    fixture.desired.extend(
        ResolvedResource::collect_resolved_with_schemas([other], &fixture.registry).unwrap(),
    );
    fixture.router.add_provider_instance(
        ProviderInstanceKey::new("test", Some("other".into())),
        InitializedProvider::unavailable(Box::new(TableProvider {
            table: HashMap::new(),
            calls: fixture.calls.clone(),
            barrier: None,
            result_count: None,
            fail_batch: true,
            updates: fixture.updates.clone(),
        })),
    );
    let references = fixture
        .prepare()
        .await
        .expect("unused batch failures cannot fail planning");
    assert!(fixture.plan(&references).unwrap().effects().is_empty());
    assert_eq!(
        fixture.calls.lock().unwrap().len(),
        2,
        "one concurrent batch per instance"
    );
    for raw in ["alias/untouched", "alias/unused-saved"] {
        assert!(matches!(
            references.values.get(&CacheKey {
                instance: ProviderInstanceKey::from(&id),
                ty: identity(),
                raw: raw.into(),
            }),
            Some(CachedOutcome::Failed(ReferenceFailure::Call(_)))
        ));
        let error = references
            .equal(
                &id,
                &identity(),
                &raw.into(),
                &"key/a".into(),
                &[
                    ReferenceOperand {
                        path: "key".into(),
                        side: ReferenceSide::Current,
                    },
                    ReferenceOperand {
                        path: "key".into(),
                        side: ReferenceSide::Desired,
                    },
                ],
            )
            .unwrap_err();
        assert!(matches!(error.source, ReferenceFailure::Call(_)));
        assert_eq!(error.path, "key");
        assert_eq!(error.raw.expose_plaintext(), raw);
    }
    assert_eq!(references.dangling_paths(&id).count(), 0);
    assert_eq!(fixture.calls.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn round8_unused_secret_values_are_never_requested_or_rejected_by_enumeration() {
    const SECRET: &str = "alias/secret-saved";
    for wrapped in [true, false] {
        let mut fixture = Fixture::new(
            string("alias/live"),
            string("key/a"),
            HashMap::from([("alias/live".into(), resolved("key/a"))]),
            false,
        );
        let value = if wrapped {
            Value::Deferred(DeferredValue::Secret(Box::new(string(SECRET))))
        } else {
            Value::Concrete(ConcreteValue::String(ValueString::diagnostic_sensitive(
                SECRET,
            )))
        };
        fixture.saved = RawSavedAttrs::from_decoded_values_for_test(HashMap::from([(
            fixture.desired[0].id.clone(),
            HashMap::from([("key".into(), value)]),
        )]))
        .lift(&fixture.registry);
        let references = fixture
            .prepare()
            .await
            .expect("an unused saved secret is not a comparison operand");
        assert!(fixture.plan(&references).unwrap().effects().is_empty());
        let calls = fixture.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert!(calls[0].iter().all(|request| request.value != SECRET));
    }
}

#[tokio::test]
async fn round8_compared_secret_reference_list_leaves_keep_typed_paths_and_redaction() {
    use crate::schema::AttributeType;
    const SECRET: &str = "alias/private-list-secret";
    for ordered in [true, false] {
        for wrapped in [true, false] {
            let secret = if wrapped {
                Value::Deferred(DeferredValue::Secret(Box::new(string(SECRET))))
            } else {
                Value::Concrete(ConcreteValue::String(ValueString::diagnostic_sensitive(
                    SECRET,
                )))
            };
            let mut fixture =
                Fixture::new(string("unused"), string("unused"), HashMap::new(), false);
            let element = AttributeType::reference(identity(), None, None);
            fixture.registry.insert(
                "test",
                crate::schema::ResourceSchema::new("keys.Resource")
                    .attribute(
                        AttributeSchema::optional(
                            "key",
                            if ordered {
                                AttributeType::list(element)
                            } else {
                                AttributeType::unordered_list(element)
                            },
                        )
                        .build(),
                    )
                    .validate()
                    .unwrap(),
            );
            reset_values(
                &mut fixture,
                Value::Concrete(ConcreteValue::List(vec![secret])),
                Value::Concrete(ConcreteValue::List(vec![string(SECRET)])),
                Value::Concrete(ConcreteValue::List(vec![string(SECRET)])),
            );
            let error = match fixture.prepare().await {
                Err(error) => error,
                Ok(_) => panic!("secret list ordered={ordered}, wrapped={wrapped}"),
            };
            assert!(matches!(
                error.source,
                ReferenceFailure::Resolution(ReferenceError::InvalidReference(_))
            ));
            assert_eq!(error.path, "key[0]");
            assert!(!format!("{error:?}").contains(SECRET));
            assert!(!error.to_string().contains(SECRET));
            assert!(fixture.calls.lock().unwrap().is_empty());
        }
    }
}

#[tokio::test]
async fn round8_write_only_record_values_are_enumerated_independently_of_saved_attributes() {
    use crate::schema::{AttributeType, StructField};
    let mut fixture = Fixture::new(
        string("unused"),
        string("unused"),
        HashMap::from([
            ("alias/desired".into(), resolved("key/a")),
            ("alias/accepted".into(), resolved("key/a")),
        ]),
        false,
    );
    let schema = crate::schema::ResourceSchema::new("keys.Resource")
        .attribute(
            AttributeSchema::optional(
                "key",
                AttributeType::list(AttributeType::struct_(
                    "Entry",
                    vec![
                        StructField::optional(
                            "kms",
                            AttributeType::reference(identity(), None, None),
                        )
                        .write_only()
                        .build(),
                        StructField::optional("mode", AttributeType::string()).build(),
                    ],
                )),
            )
            .build(),
        )
        .validate()
        .unwrap();
    let config = |key: Option<&str>| {
        let mut values = indexmap::IndexMap::from([("mode".into(), string("same"))]);
        if let Some(key) = key {
            values.insert("kms".into(), string(key));
        }
        Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
            ConcreteValue::Map(values),
        )]))
    };
    let accepted = indexmap::IndexMap::from([("key".into(), config(Some("alias/accepted")))]);
    let mut record = NestedWriteOnlyRecord::new();
    record
        .record_sent(schema.as_schema(), ["key"], &accepted, None)
        .unwrap();
    fixture.registry.insert("test", schema);
    reset_values(
        &mut fixture,
        config(Some("alias/desired")),
        config(None),
        config(None),
    );
    let id = fixture.desired[0].id.clone();
    fixture.saved = RawSavedAttrs::from_persisted(
        HashMap::from([(
            id.clone(),
            PersistedAttributes::from_typed_persisted_wire_values(HashMap::from([(
                "key".into(),
                config(None),
            )]))
            .unwrap(),
        )]),
        HashMap::from([(id, record)]),
    )
    .lift(&fixture.registry);
    let references = fixture
        .prepare()
        .await
        .expect("the accepted write-only value needs no extra replay");
    assert!(fixture.plan(&references).unwrap().effects().is_empty());
    let calls = fixture.calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0]
            .iter()
            .map(|request| request.value.as_str())
            .collect::<BTreeSet<_>>(),
        BTreeSet::from(["alias/desired", "alias/accepted"])
    );
}

#[tokio::test]
async fn unequal_raw_references_compare_only_canonical_identity_and_preserve_raw_values() {
    for current in ["key-id-a", "key/region-a/account-a/a"] {
        let fixture = Fixture::new(
            string("alias/live"),
            string(current),
            HashMap::from([
                ("alias/live".into(), resolved("key/region-a/account-a/a")),
                (current.into(), resolved("key/region-a/account-a/a")),
            ]),
            false,
        );
        let witness = fixture.prepare().await.unwrap();
        assert!(fixture.plan(&witness).unwrap().effects().is_empty());
        assert_eq!(fixture.desired[0].attributes["key"], string("alias/live"));
        let calls = fixture.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].len(), 2);
        let stability = crate::differ::managed_binding_stability(
            &fixture.desired,
            &fixture.current,
            &fixture.registry,
            &fixture.saved,
            &HashMap::new(),
            &fixture.router,
            &witness,
        )
        .unwrap();
        assert_eq!(
            stability[&fixture.desired[0].id],
            crate::binding_index::ManagedBindingStability::NoPlannedEffect
        );
    }
}

#[tokio::test]
async fn unknown_and_deferred_reference_values_are_not_resolved() {
    for unknown in [
        Value::Deferred(DeferredValue::Unknown(
            ApplyTimeUnknown::UpstreamAllExports {
                binding: crate::resource::absolute_binding_address_for_test("upstream"),
            },
        )),
        Value::root_resource_ref("producer", "key", Vec::new()),
    ] {
        for desired_unknown in [true, false] {
            let (desired, current) = if desired_unknown {
                (unknown.clone(), string("key/a"))
            } else {
                (string("alias/live"), unknown.clone())
            };
            let fixture = Fixture::new(desired, current, HashMap::new(), false);
            let witness = fixture.prepare().await.unwrap();
            fixture.plan(&witness).unwrap();
            assert!(fixture.calls.lock().unwrap().is_empty());
        }
    }
}

#[tokio::test]
async fn arn_returning_resource_detects_repoint_and_region_account_differences() {
    for create_only in [true, false] {
        for canonical in [
            "key/region-a/account-a/b",
            "key/region-b/account-a/a",
            "key/region-a/account-b/a",
        ] {
            let fixture = Fixture::new(
                string("alias/live"),
                string("key/region-a/account-a/a"),
                HashMap::from([("alias/live".into(), resolved(canonical))]),
                create_only,
            );
            let witness = fixture.prepare().await.unwrap();
            let plan = fixture.plan(&witness).unwrap();
            assert!(!plan.effects().is_empty());
            assert_eq!(fixture.desired[0].attributes["key"], string("alias/live"));
        }
    }
}

#[tokio::test]
async fn every_d6_table_cell_is_enforced_with_typed_sources() {
    for desired_side in [true, false] {
        for kind in ["resolved", "not_found", "denied", "transient", "invalid"] {
            let result = match kind {
                "resolved" => resolved("key/same"),
                "not_found" => Ok(wire::ReferenceOutcome::NotFound(error())),
                "denied" => Err(wire::ReferenceError::AccessDenied(error())),
                "transient" => Err(wire::ReferenceError::Transient(error())),
                "invalid" => Err(wire::ReferenceError::InvalidReference(error())),
                _ => unreachable!(),
            };
            let raw = if desired_side {
                "alias/desired"
            } else {
                "alias/current"
            };
            let mut table = HashMap::from([
                ("alias/desired".into(), resolved("key/same")),
                ("alias/current".into(), resolved("key/same")),
            ]);
            table.insert(raw.into(), result);
            let fixture = Fixture::new(
                string("alias/desired"),
                string("alias/current"),
                table,
                false,
            );
            match fixture.prepare().await {
                Ok(witness) => {
                    assert!(kind == "resolved" || (!desired_side && kind == "not_found"));
                    let plan = fixture.plan(&witness).unwrap();
                    assert_eq!(plan.effects().is_empty(), kind == "resolved");
                    assert_eq!(
                        witness
                            .dangling_paths(&fixture.desired[0].id)
                            .collect::<Vec<_>>(),
                        if kind == "not_found" {
                            vec!["key"]
                        } else {
                            vec![]
                        }
                    );
                }
                Err(error) => {
                    assert!(
                        kind != "resolved" && (desired_side || kind != "not_found"),
                        "{error:?}"
                    );
                    assert!(matches!(
                        (&error.source, kind),
                        (ReferenceFailure::DesiredNotFound(_), "not_found")
                            | (
                                ReferenceFailure::Resolution(ReferenceError::AccessDenied(_)),
                                "denied"
                            )
                            | (
                                ReferenceFailure::Resolution(ReferenceError::Transient(_)),
                                "transient"
                            )
                            | (
                                ReferenceFailure::Resolution(ReferenceError::InvalidReference(_)),
                                "invalid"
                            )
                    ));
                    assert_eq!(error.path, "key");
                    assert_eq!(error.raw.expose_plaintext(), raw);
                    assert_eq!(error.instance, ProviderInstanceKey::new("test", None));
                    let mut chain: Option<&(dyn std::error::Error + 'static)> = Some(&error);
                    let mut text = String::new();
                    while let Some(error) = chain {
                        text.push_str(&error.to_string());
                        chain = error.source();
                    }
                    for required in [
                        "example",
                        "key",
                        raw,
                        "DeniedCode",
                        "request-123",
                        "transport diagnostic",
                    ] {
                        assert!(text.contains(required), "missing {required}: {text}");
                    }
                }
            }
        }
    }
}

#[tokio::test]
async fn missing_needed_witness_entry_is_typed_error_in_plan_and_binding_stability() {
    let fixture = Fixture::new(string("alias/live"), string("key/a"), HashMap::new(), false);
    let witness = prepare_empty_for_tests();
    let error = fixture.plan(&witness).unwrap_err();
    let crate::differ::PlanComparisonError::Reference(source) = error else {
        panic!("expected typed reference error")
    };
    assert!(matches!(source.source, ReferenceFailure::MissingWitness));
    assert_eq!(source.path, "key");
    assert!(source.to_string().contains("attribute key,"));
    let Current::Present(current) = fixture.current[&fixture.desired[0].id].current() else {
        panic!("present fixture")
    };
    let comparison = fixture
        .router
        .comparison_for(&fixture.registry, &fixture.desired[0].id)
        .unwrap();
    let error = comparison
        .context()
        .with_references(&witness, &fixture.desired[0].id, |context| {
            crate::differ::diff(
                &fixture.desired[0],
                current.state(),
                &fixture.saved,
                None,
                comparison,
                context,
            )
        })
        .unwrap_err();
    assert!(matches!(error.source, ReferenceFailure::MissingWitness));

    assert!(
        crate::differ::managed_binding_stability(
            &fixture.desired,
            &fixture.current,
            &fixture.registry,
            &fixture.saved,
            &HashMap::new(),
            &fixture.router,
            &witness
        )
        .is_err()
    );
}

#[tokio::test]
async fn resolution_cache_is_per_run_and_includes_type_and_provider_instance() {
    let mut fixture = Fixture::new(
        string("alias/live"),
        string("key/a"),
        HashMap::from([("alias/live".into(), resolved("key/a"))]),
        false,
    );
    let mut run = ReferencePreparation::default();
    run.prepare(
        &fixture.desired,
        &fixture.current,
        &fixture.registry,
        &fixture.saved,
        &fixture.router,
    )
    .await
    .unwrap();
    run.prepare(
        &fixture.desired,
        &fixture.current,
        &fixture.registry,
        &fixture.saved,
        &fixture.router,
    )
    .await
    .unwrap();
    assert_eq!(fixture.calls.lock().unwrap().len(), 1);
    fixture.prepare().await.unwrap();
    assert_eq!(
        fixture.calls.lock().unwrap().len(),
        2,
        "another run resolves again"
    );
    fixture.router.add_provider_instance(
        ProviderInstanceKey::new("test", Some("other".into())),
        InitializedProvider::unavailable(Box::new(TableProvider {
            table: HashMap::from([("alias/live".into(), resolved("key/b"))]),
            calls: fixture.calls.clone(),
            barrier: None,
            result_count: None,
            fail_batch: false,
            updates: fixture.updates.clone(),
        })),
    );
    let mut other = fixture.desired[0].as_resource().clone();
    other.id.provider_instance = Some("other".into());
    let mut state = State::existing(
        other.id.clone(),
        HashMap::from([("key".into(), string("key/a"))]),
    );
    state.identifier = Some("resource-123".into());
    let other_current = crate::resource::into_plan_input_current_map(
        HashMap::from([(other.id.clone(), Current::from_state(state))]),
        &fixture.registry,
        &[other.clone()],
    );
    let other =
        ResolvedResource::collect_resolved_with_schemas([other], &fixture.registry).unwrap();
    let witness = run
        .prepare(
            &other,
            &other_current,
            &fixture.registry,
            &fixture.saved,
            &fixture.router,
        )
        .await
        .unwrap();
    assert!(
        !witness
            .equal(
                &other[0].id,
                &identity(),
                &"alias/live".into(),
                &"key/a".into(),
                &[
                    ReferenceOperand {
                        path: "key".into(),
                        side: ReferenceSide::Desired
                    },
                    ReferenceOperand {
                        path: "key".into(),
                        side: ReferenceSide::Current
                    }
                ],
            )
            .unwrap()
    );
    assert_eq!(
        fixture.calls.lock().unwrap().len(),
        3,
        "same raw value in another instance resolves separately"
    );
}

#[tokio::test]
async fn accepted_write_only_reference_fallback_uses_the_same_identity_rules() {
    let mut fixture = Fixture::new(
        string("alias/live"),
        string("key/a"),
        HashMap::from([("alias/live".into(), resolved("key/a"))]),
        false,
    );
    fixture.registry.insert("test", schema(false, true));
    fixture.saved = RawSavedAttrs::from_decoded_values_for_test(HashMap::from([(
        fixture.desired[0].id.clone(),
        HashMap::from([("key".into(), string("key/a"))]),
    )]))
    .lift(&fixture.registry);
    let mut state = State::existing(fixture.desired[0].id.clone(), HashMap::new());
    state.identifier = Some("resource-123".into());
    fixture.current = crate::resource::into_plan_input_current_map(
        HashMap::from([(state.id.clone(), Current::from_state(state))]),
        &fixture.registry,
        &[fixture.desired[0].as_resource().clone()],
    );
    let witness = fixture.prepare().await.unwrap();
    assert!(fixture.plan(&witness).unwrap().effects().is_empty());
    assert_eq!(
        fixture.saved.get(&fixture.desired[0].id).unwrap()["key"],
        string("key/a")
    );
}

#[tokio::test]
async fn ignored_reference_differences_do_not_resolve() {
    let mut fixture = Fixture::new(
        string("alias/live"),
        string("key/a"),
        HashMap::from([(
            "alias/live".into(),
            Err(wire::ReferenceError::AccessDenied(error())),
        )]),
        false,
    );
    let mut resource = fixture.desired[0].as_resource().clone();
    resource.directives.ignore_changes = vec![crate::resource::ParsedIgnorePath::new(vec![
        crate::resource::ParsedIgnorePathSegment::Attribute("key".into()),
    ])];
    fixture.desired =
        ResolvedResource::collect_resolved_with_schemas([resource], &fixture.registry).unwrap();
    let witness = fixture.prepare().await.unwrap();
    assert!(fixture.calls.lock().unwrap().is_empty());
    assert!(fixture.plan(&witness).unwrap().effects().is_empty());
}

#[tokio::test]
async fn reference_lists_compare_canonical_values_across_list_representations() {
    let mut fixture = Fixture::new(
        string("alias/live"),
        string("key/a"),
        HashMap::from([("alias/live".into(), resolved("key/a"))]),
        false,
    );
    fixture.registry.insert(
        "test",
        crate::schema::ResourceSchema::new("keys.Resource")
            .attribute(
                AttributeSchema::optional(
                    "key",
                    crate::schema::AttributeType::list(crate::schema::AttributeType::reference(
                        identity(),
                        None,
                        None,
                    )),
                )
                .build(),
            )
            .validate()
            .unwrap(),
    );
    let mut resource = fixture.desired[0].as_resource().clone();
    resource.attributes.insert(
        "key".into(),
        Value::Concrete(ConcreteValue::List(vec![string("alias/live")])),
    );
    fixture.desired =
        ResolvedResource::collect_resolved_with_schemas([resource.clone()], &fixture.registry)
            .unwrap();
    let mut state = State::existing(
        resource.id.clone(),
        HashMap::from([(
            "key".into(),
            Value::Concrete(ConcreteValue::StringList(vec!["key/a".into()])),
        )]),
    );
    state.identifier = Some("resource-123".into());
    fixture.current = crate::resource::into_plan_input_current_map(
        HashMap::from([(state.id.clone(), Current::from_state(state))]),
        &fixture.registry,
        &[resource],
    );
    let witness = fixture.prepare().await.unwrap();
    assert!(fixture.plan(&witness).unwrap().effects().is_empty());
    assert_eq!(fixture.calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn nested_write_only_reference_resolves_against_accepted_leaf_when_read_omits_the_object() {
    use crate::schema::{AttributeType, StructField};
    let mut fixture = Fixture::new(
        string("alias/live"),
        string("key/a"),
        HashMap::from([("alias/live".into(), resolved("key/a"))]),
        false,
    );
    let schema = crate::schema::ResourceSchema::new("keys.Resource")
        .attribute(
            AttributeSchema::optional(
                "key",
                AttributeType::struct_(
                    "Settings",
                    vec![
                        StructField::optional(
                            "reference",
                            AttributeType::reference(identity(), None, None),
                        )
                        .write_only()
                        .build(),
                    ],
                ),
            )
            .build(),
        )
        .validate()
        .unwrap();
    let mut resource = fixture.desired[0].as_resource().clone();
    resource.attributes.insert(
        "key".into(),
        Value::Concrete(ConcreteValue::Map(indexmap::IndexMap::from([(
            "reference".into(),
            string("alias/live"),
        )]))),
    );
    let accepted = indexmap::IndexMap::from([(
        "key".into(),
        Value::Concrete(ConcreteValue::Map(indexmap::IndexMap::from([(
            "reference".into(),
            string("key/a"),
        )]))),
    )]);
    let mut record = NestedWriteOnlyRecord::new();
    record
        .record_sent(schema.as_schema(), ["key"], &accepted, None)
        .unwrap();
    let saved = PersistedAttributes::from_persisted_values(
        accepted
            .iter()
            .map(|(key, value)| {
                (
                    key.clone(),
                    crate::value::PersistedValue::encode_provider_value(value, None).unwrap(),
                )
            })
            .collect(),
    )
    .unwrap();
    fixture.registry.insert("test", schema);
    fixture.saved = RawSavedAttrs::from_persisted(
        HashMap::from([(resource.id.clone(), saved)]),
        HashMap::from([(resource.id.clone(), record)]),
    )
    .lift(&fixture.registry);
    fixture.desired =
        ResolvedResource::collect_resolved_with_schemas([resource.clone()], &fixture.registry)
            .unwrap();
    let mut state = State::existing(resource.id.clone(), HashMap::new());
    state.identifier = Some("resource-123".into());
    fixture.current = crate::resource::into_plan_input_current_map(
        HashMap::from([(state.id.clone(), Current::from_state(state))]),
        &fixture.registry,
        &[resource],
    );
    let witness = fixture.prepare().await.unwrap();
    assert!(fixture.plan(&witness).unwrap().effects().is_empty());
    assert_eq!(fixture.calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn repeated_leaves_are_deduplicated_per_type_in_a_positional_batch() {
    use crate::schema::AttributeType;
    let mut fixture = Fixture::new(
        string("alias/live"),
        string("key/a"),
        HashMap::from([("alias/live".into(), resolved("key/a"))]),
        false,
    );
    fixture.registry.insert(
        "test",
        crate::schema::ResourceSchema::new("keys.Resource")
            .attribute(
                AttributeSchema::optional("key", AttributeType::reference(identity(), None, None))
                    .build(),
            )
            .attribute(
                AttributeSchema::optional(
                    "other",
                    AttributeType::reference(
                        TypeIdentity::new(Some("test"), ["keys", "Other"], "Reference"),
                        None,
                        None,
                    ),
                )
                .build(),
            )
            .validate()
            .unwrap(),
    );
    let mut first = fixture.desired[0].as_resource().clone();
    first
        .attributes
        .insert("other".into(), string("alias/live"));
    let mut second = first.clone().with_root_binding("second");
    second.id.set_identity("second".into());
    let resources = vec![first, second];
    let current = resources
        .iter()
        .map(|resource| {
            let mut state = State::existing(
                resource.id.clone(),
                HashMap::from([
                    ("key".into(), string("key/a")),
                    ("other".into(), string("key/a")),
                ]),
            );
            state.identifier = Some("resource-123".into());
            (resource.id.clone(), Current::from_state(state))
        })
        .collect();
    fixture.current =
        crate::resource::into_plan_input_current_map(current, &fixture.registry, &resources);
    fixture.desired =
        ResolvedResource::collect_resolved_with_schemas(resources, &fixture.registry).unwrap();
    let witness = fixture.prepare().await.unwrap();
    assert!(fixture.plan(&witness).unwrap().effects().is_empty());
    let calls = fixture.calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0].len(),
        4,
        "two raw values times two identities, shared across resources"
    );
}

#[tokio::test]
async fn unordered_reference_list_elements_share_one_resolution_path() {
    use crate::schema::AttributeType;
    let list = |items: &[&str]| {
        Value::Concrete(ConcreteValue::List(
            items.iter().map(|item| string(item)).collect(),
        ))
    };
    let mut fixture = Fixture::new(
        list(&["alias/a", "alias/b"]),
        list(&["alias/b", "alias/a"]),
        HashMap::from([
            ("alias/a".into(), resolved("key/same")),
            ("alias/b".into(), resolved("key/same")),
        ]),
        false,
    );
    fixture.registry.insert(
        "test",
        crate::schema::ResourceSchema::new("keys.Resource")
            .attribute(
                AttributeSchema::optional(
                    "key",
                    AttributeType::unordered_list(AttributeType::reference(identity(), None, None)),
                )
                .build(),
            )
            .validate()
            .unwrap(),
    );
    let resource = fixture.desired[0].as_resource().clone();
    fixture.desired =
        ResolvedResource::collect_resolved_with_schemas([resource.clone()], &fixture.registry)
            .unwrap();
    let state = State {
        id: resource.id.clone(),
        identifier: Some("resource-123".into()),
        attributes: HashMap::from([("key".into(), list(&["alias/b", "alias/a"]))]),
        exists: true,
        dependency_bindings: BTreeSet::new(),
        partial_read: None,
    };
    fixture.current = crate::resource::into_plan_input_current_map(
        HashMap::from([(state.id.clone(), Current::from_state(state))]),
        &fixture.registry,
        &[resource],
    );
    let witness = fixture.prepare().await.unwrap();
    assert!(fixture.plan(&witness).unwrap().effects().is_empty());
    assert!(fixture.calls.lock().unwrap().is_empty());
}

fn reset_values(fixture: &mut Fixture, desired: Value, current: Value, saved: Value) {
    let mut resource = fixture.desired[0].as_resource().clone();
    resource.attributes.insert("key".into(), desired);
    fixture.desired =
        ResolvedResource::collect_resolved_with_schemas([resource.clone()], &fixture.registry)
            .unwrap();
    let mut state = State::existing(
        resource.id.clone(),
        HashMap::from([("key".into(), current)]),
    );
    state.identifier = Some("resource-123".into());
    fixture.current = crate::resource::into_plan_input_current_map(
        HashMap::from([(resource.id.clone(), Current::from_state(state))]),
        &fixture.registry,
        &[resource.clone()],
    );
    fixture.saved = RawSavedAttrs::from_decoded_values_for_test(HashMap::from([(
        resource.id,
        HashMap::from([("key".into(), saved)]),
    )]))
    .lift(&fixture.registry);
}

fn encryption_fixture(desired_key: Value, ignored: bool) -> Fixture {
    use crate::resource::{ParsedIgnorePath, ParsedIgnorePathSegment};
    use crate::schema::{AttributeType, StructField};
    let mut fixture = Fixture::new(string("unused"), string("unused"), HashMap::new(), false);
    fixture.registry.insert(
        "test",
        crate::schema::ResourceSchema::new("keys.Resource")
            .attribute(
                AttributeSchema::optional(
                    "encryption",
                    AttributeType::struct_(
                        "Encryption",
                        vec![
                            StructField::optional(
                                "kms_key_id",
                                AttributeType::reference(identity(), None, None),
                            )
                            .create_only()
                            .build(),
                            StructField::optional("mode", AttributeType::string()).build(),
                        ],
                    ),
                )
                .build(),
            )
            .validate()
            .unwrap(),
    );
    let config = |key, mode| {
        Value::Concrete(ConcreteValue::Map(indexmap::IndexMap::from([
            ("kms_key_id".into(), key),
            ("mode".into(), string(mode)),
        ])))
    };
    let mut resource = fixture.desired[0].as_resource().clone();
    resource.attributes =
        indexmap::IndexMap::from([("encryption".into(), config(desired_key, "new"))]);
    if ignored {
        resource.directives.ignore_changes = vec![ParsedIgnorePath::new(vec![
            ParsedIgnorePathSegment::Attribute("encryption".into()),
            ParsedIgnorePathSegment::Attribute("kms_key_id".into()),
        ])];
    }
    fixture.desired =
        ResolvedResource::collect_resolved_with_schemas([resource.clone()], &fixture.registry)
            .unwrap();
    let attributes = HashMap::from([("encryption".into(), config(string("key/a"), "old"))]);
    fixture.current = crate::resource::into_plan_input_current_map(
        HashMap::from([(
            resource.id.clone(),
            Current::from_state(
                State::existing(resource.id.clone(), attributes.clone())
                    .with_identifier("resource-123"),
            ),
        )]),
        &fixture.registry,
        &[resource.clone()],
    );
    fixture.saved =
        RawSavedAttrs::from_decoded_values_for_test(HashMap::from([(resource.id, attributes)]))
            .lift(&fixture.registry);
    fixture
}

#[tokio::test]
async fn round3_ignored_nested_create_only_reference_with_sibling_change_is_an_update() {
    let fixture = encryption_fixture(string("alias/ignored"), true);
    let witness = fixture.prepare().await.unwrap();
    assert!(fixture.calls.lock().unwrap().is_empty());
    let plan = fixture.plan(&witness).unwrap();
    assert!(plan.errors().is_empty(), "{:?}", plan.errors());
    assert!(
        matches!(plan.effects(), [crate::effect::Effect::Update { changed_attributes, .. }]
        if changed_attributes == &["encryption"])
    );
    let stability = crate::differ::managed_binding_stability(
        &fixture.desired,
        &fixture.current,
        &fixture.registry,
        &fixture.saved,
        &HashMap::new(),
        &fixture.router,
        &witness,
    )
    .unwrap();
    assert!(matches!(
        stability[&fixture.desired[0].id],
        crate::binding_index::ManagedBindingStability::Updating { .. }
    ));
}

#[tokio::test]
async fn round3_unordered_list_index_ignore_precedes_projection_pairing_and_saved_merge() {
    use crate::resource::{
        ParsedIgnorePath, ParsedIgnorePathSegment as Segment, ResolvedIgnoreChanges,
    };
    use crate::schema::{AttributeType, LeafPath, LeafPathSegment, StructField};
    let mut fixture = Fixture::new(
        string("unused"),
        string("unused"),
        HashMap::from([
            ("alias/live".into(), resolved("key/b")),
            ("alias/saved".into(), resolved("key/b")),
        ]),
        false,
    );
    fixture.registry.insert(
        "test",
        crate::schema::ResourceSchema::new("keys.Resource")
            .attribute(
                AttributeSchema::optional(
                    "key",
                    AttributeType::unordered_list(AttributeType::struct_(
                        "Rule",
                        vec![
                            StructField::optional("id", AttributeType::string()).build(),
                            StructField::optional(
                                "reference",
                                AttributeType::reference(identity(), None, None),
                            )
                            .create_only()
                            .build(),
                            StructField::optional("mode", AttributeType::string()).build(),
                        ],
                    )),
                )
                .build(),
            )
            .validate()
            .unwrap(),
    );
    let list = |ignored: &str, live: &str, mode: &str| {
        Value::Concrete(ConcreteValue::List(vec![
            Value::Concrete(ConcreteValue::Map(indexmap::IndexMap::from([
                ("id".into(), string("ignored")),
                ("reference".into(), string(ignored)),
                ("mode".into(), string(mode)),
            ]))),
            Value::Concrete(ConcreteValue::Map(indexmap::IndexMap::from([
                ("id".into(), string("live")),
                ("reference".into(), string(live)),
                ("mode".into(), string("stable")),
            ]))),
        ]))
    };
    reset_values(
        &mut fixture,
        list("alias/ignored-desired", "alias/live", "new"),
        list("alias/ignored-read", "key/b", "old"),
        list("alias/ignored-saved", "alias/saved", "old"),
    );
    let mut resource = fixture.desired[0].as_resource().clone();
    let path = ParsedIgnorePath::new(vec![
        Segment::Attribute("key".into()),
        Segment::ListIndex(0),
        Segment::Attribute("reference".into()),
    ]);
    resource.directives.ignore_changes = vec![path.clone()];
    resource.resolved_ignore_changes = ResolvedIgnoreChanges::indexed_for_test(
        "key",
        LeafPath::from_segments(vec![
            LeafPathSegment::ListIndex(0),
            LeafPathSegment::StructField("reference".into()),
        ]),
        path,
    );
    fixture.desired = vec![ResolvedResource::new(resource)];
    let explicit = HashMap::from([(
        fixture.desired[0].id.clone(),
        crate::explicit::build_from_resource(fixture.desired[0].as_resource()),
    )]);
    let references = fixture.prepare().await.unwrap();
    let plan = fixture.plan_with_explicit(&references, &explicit).unwrap();
    assert!(plan.errors().is_empty(), "{:?}", plan.errors());
    assert!(
        matches!(plan.effects(), [crate::effect::Effect::Update { changed_attributes, .. }] if changed_attributes == &["key"])
    );
    let persisted = PersistedAttributes::from_persisted_values(
        fixture
            .saved
            .get(&fixture.desired[0].id)
            .unwrap()
            .iter()
            .map(|(key, value)| {
                (
                    key.clone(),
                    crate::value::PersistedValue::encode_provider_value(value, None).unwrap(),
                )
            })
            .collect(),
    )
    .unwrap();
    let Current::Present(current) = fixture.current[&fixture.desired[0].id].current() else {
        unreachable!()
    };
    let mut historical = crate::detail_rows::HistoricalDisplayAttributes::for_plan(&plan);
    historical.set_update_from_provider_and_persisted(
        fixture.desired[0].id.clone(),
        &current.state().attributes,
        Some(&persisted),
    );
    crate::detail_rows::build_detail_rows_by_instance(
        &plan.effects()[0],
        Some(&fixture.registry),
        crate::detail_rows::DetailLevel::Full,
        &historical,
        Some(&explicit),
        &fixture.router,
        crate::detail_rows::DetailComparisons::Planning(&references),
    )
    .expect("detail pairing must use the prepared ignore mask too");
    let calls = fixture.calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0]
            .iter()
            .map(|r| r.value.as_str())
            .collect::<BTreeSet<_>>(),
        BTreeSet::from(["alias/live", "alias/saved", "key/b"])
    );
}

#[test]
fn round3_comparator_reference_errors_render_the_nested_attribute_path() {
    for sensitive in [false, true] {
        let raw = if sensitive {
            Value::Concrete(ConcreteValue::String(ValueString::diagnostic_sensitive(
                "alias/private",
            )))
        } else {
            string("alias/unprepared")
        };
        let fixture = encryption_fixture(raw, false);
        let error = fixture.plan(&prepare_empty_for_tests()).unwrap_err();
        let crate::differ::PlanComparisonError::Reference(error) = error else {
            panic!("expected a reference comparison error")
        };
        assert!(matches!(
            (&error.source, sensitive),
            (ReferenceFailure::MissingWitness, false)
                | (
                    ReferenceFailure::Resolution(ReferenceError::InvalidReference(_)),
                    true
                )
        ));
        assert_eq!(error.path, "encryption.kms_key_id");
        assert!(
            error
                .to_string()
                .contains("attribute encryption.kms_key_id,")
        );
        if sensitive {
            assert!(!error.to_string().contains("alias/private"));
        }
    }
}

#[tokio::test]
async fn round3_native_reference_batches_reject_short_and_long_results() {
    for count in [0, 1, 3] {
        let mut fixture =
            Fixture::new(string("alias/live"), string("key/a"), HashMap::new(), false);
        fixture.router.add_provider(
            "test".into(),
            InitializedProvider::unavailable(Box::new(TableProvider {
                table: HashMap::new(),
                calls: fixture.calls.clone(),
                barrier: None,
                result_count: Some(count),
                fail_batch: false,
                updates: fixture.updates.clone(),
            })),
        );
        let error = fixture
            .prepare()
            .await
            .expect_err("batch length must be checked before zip");
        assert!(
            matches!(
                error.source,
                ReferenceFailure::Resolution(ReferenceError::InvalidReference(_))
            ),
            "{error:?}"
        );
        assert_eq!(error.path, "key");
    }
}

#[tokio::test]
async fn round3_apply_concrete_plan_reference_reresolves_without_widening_equivalent_patch() {
    use crate::binding_index::{BindingValueSource, ResolvedBindings};
    use crate::executor::{LiveExecutionInput, ProviderCheckInputs, UnresolvedResource};
    use crate::schema::AttributeType;
    for (create_only, target_key) in [
        (false, "key/a"),
        (true, "key/a"),
        (false, "key/b"),
        (true, "key/b"),
    ] {
        let mut fixture = Fixture::new(
            string("key/a"),
            string("key/a"),
            HashMap::from([("alias/live".into(), resolved(target_key))]),
            create_only,
        );
        let mut declaration = crate::schema::ResourceSchema::new("keys.Resource")
            .attribute(AttributeSchema::optional("description", AttributeType::string()).build());
        let key =
            AttributeSchema::optional("key", AttributeType::reference(identity(), None, None));
        declaration = declaration.attribute(if create_only {
            key.create_only().build()
        } else {
            key.build()
        });
        fixture
            .registry
            .insert("test", declaration.validate().unwrap());
        let mut target = fixture.desired[0].as_resource().clone();
        target
            .attributes
            .insert("description".into(), string("new"));
        let from = State::existing(
            target.id.clone(),
            HashMap::from([
                ("key".into(), string("key/a")),
                ("description".into(), string("old")),
            ]),
        )
        .with_identifier("resource-123");
        fixture.current = crate::resource::into_plan_input_current_map(
            HashMap::from([(target.id.clone(), Current::from_state(from))]),
            &fixture.registry,
            &[target.clone()],
        );
        fixture.desired =
            ResolvedResource::collect_resolved_with_schemas([target.clone()], &fixture.registry)
                .unwrap();
        let references = fixture.prepare().await.unwrap();
        let plan = fixture.plan(&references).unwrap();
        assert!(
            matches!(plan.effects(), [crate::effect::Effect::Update { changed_attributes, .. }]
            if changed_attributes == &["description"])
        );
        assert!(
            fixture.calls.lock().unwrap().is_empty(),
            "the reference is concrete and byte equal at plan"
        );
        let mut origin = target.clone();
        origin.attributes.insert(
            "key".into(),
            Value::root_resource_ref("dependency", "key", vec![]),
        );
        let origins = HashMap::from([(
            target.id.clone(),
            UnresolvedResource::from_pre_resolve(origin),
        )]);
        let mut bindings = ResolvedBindings::default();
        bindings.set(
            "dependency",
            HashMap::from([("key".into(), string("alias/live"))]),
            BindingValueSource::Local,
        );
        let outcome = crate::executor::execute_live_plan(
            &fixture.router,
            LiveExecutionInput {
                plan: &plan,
                provider_check_inputs: ProviderCheckInputs::PlanNormalized {
                    resource_origins: &origins,
                    data_source_origins: &[],
                },
                compositions: &[],
                bindings,
                managed_current: HashMap::new(),
                data_source_values: crate::data_source_value::ExecutableDataSourceValues::empty(),
                normalizer: &NoopNormalizer,
                provider_configs: &[],
                factories: &[],
                schemas: &fixture.registry,
                parallelism: std::num::NonZeroUsize::new(1).unwrap(),
                precompleted_success_count: 0,
                checkpoint_sink: crate::executor::ExecutionCheckpointSink::noop(),
            },
            &crate::executor::NoopExecutionObserver,
            crate::shutdown::ShutdownToken::running(),
        )
        .await;
        let crate::executor::ExecutionOutcome::Completed(result) = outcome else {
            panic!("execution cancelled")
        };
        if create_only && target_key == "key/b" {
            assert_eq!(
                result.failure_count(),
                1,
                "a newly changed create-only identity requires replanning"
            );
            assert!(fixture.updates.lock().unwrap().is_empty());
            let crate::executor::EffectFailureKind::Execution { error } =
                result.failures()[0].kind()
            else {
                panic!("expected an augmentation error")
            };
            let mut cause = Some(error.as_error() as &(dyn std::error::Error + 'static));
            let mut typed_change = false;
            while let Some(error) = cause {
                if let Some(change) =
                    error.downcast_ref::<crate::differ::ApplyTimeCreateOnlyChange>()
                {
                    assert_eq!(change.resource.as_ref(), &target.id);
                    assert_eq!(change.attributes, ["key"]);
                    typed_change = true;
                }
                cause = error.source();
            }
            assert!(
                typed_change,
                "the create-only guard must retain a typed cause"
            );
            assert!(
                format!("{:?}", result.failures())
                    .to_ascii_lowercase()
                    .contains("create-only")
            );
            continue;
        }
        assert_eq!(result.success_count, 1, "{:?}", result.failures());
        let updates = fixture.updates.lock().unwrap();
        assert_eq!(updates.len(), 1);
        let keys = updates[0]
            .patch()
            .ops
            .iter()
            .map(PatchOp::key)
            .collect::<Vec<_>>();
        assert_eq!(
            keys,
            if target_key == "key/a" {
                vec!["description"]
            } else {
                vec!["description", "key"]
            }
        );
        if target_key == "key/b" {
            assert_eq!(
                updates[0]
                    .patch()
                    .ops
                    .iter()
                    .find(|op| op.key() == "key")
                    .unwrap()
                    .sent_value(),
                Some(&string("alias/live"))
            );
        }
        assert_eq!(
            fixture.calls.lock().unwrap().len(),
            1,
            "apply must prepare fresh evidence"
        );
    }
}

#[tokio::test]
async fn round3_reference_list_writeback_preserves_authored_fields_across_alias_and_arn() {
    use crate::explicit::ExplicitFields;
    use crate::schema::{AttributeType, StructField};
    let mut fixture = Fixture::new(
        string("unused"),
        string("unused"),
        HashMap::from([("alias/live".into(), resolved("key/a"))]),
        false,
    );
    fixture.registry.insert(
        "test",
        crate::schema::ResourceSchema::new("keys.Resource")
            .attribute(
                AttributeSchema::optional(
                    "key",
                    AttributeType::unordered_list(AttributeType::struct_(
                        "Rule",
                        vec![
                            StructField::optional(
                                "reference",
                                AttributeType::reference(identity(), None, None),
                            )
                            .build(),
                            StructField::optional("provider_default", AttributeType::string())
                                .build(),
                        ],
                    )),
                )
                .build(),
            )
            .validate()
            .unwrap(),
    );
    let list = |raw: &str, default: bool| {
        let mut fields = indexmap::IndexMap::from([("reference".into(), string(raw))]);
        if default {
            fields.insert("provider_default".into(), string("generated"));
        }
        Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
            ConcreteValue::Map(fields),
        )]))
    };
    reset_values(
        &mut fixture,
        list("alias/live", false),
        list("key/a", true),
        list("alias/live", false),
    );
    let prior = crate::explicit::build_from_resource(fixture.desired[0].as_resource());
    let references = fixture.prepare().await.unwrap();
    let comparison = fixture
        .router
        .comparison_for(&fixture.registry, &fixture.desired[0].id)
        .unwrap();
    let carried = crate::explicit::build_carried_from_resource_for_stored_values(
        fixture.desired[0].as_resource(),
        fixture.saved.get(&fixture.desired[0].id).unwrap(),
        &prior,
        &match fixture.current[&fixture.desired[0].id].current() {
            Current::Present(current) => current.state().attributes.clone(),
            _ => unreachable!(),
        },
        Some(comparison.owner_schema().schema().as_schema()),
        &comparison
            .context()
            .authoring(Some(&references), &fixture.desired[0].id),
    );
    let ExplicitFields::Struct { children } = carried else {
        panic!("resource authoring")
    };
    let ExplicitFields::ListElements { elements } = &children["key"] else {
        panic!("list authoring")
    };
    assert!(
        matches!(&elements[0], ExplicitFields::Struct { children }
        if children.contains_key("reference") && !children.contains_key("provider_default")),
        "{elements:?}"
    );
    let calls_before = fixture.calls.lock().unwrap().len();
    let fallback = crate::explicit::build_carried_from_resource_for_stored_values(
        fixture.desired[0].as_resource(),
        fixture.saved.get(&fixture.desired[0].id).unwrap(),
        &prior,
        &match fixture.current[&fixture.desired[0].id].current() {
            Current::Present(current) => current.state().attributes.clone(),
            _ => unreachable!(),
        },
        Some(comparison.owner_schema().schema().as_schema()),
        &comparison.context().authoring(None, &fixture.desired[0].id),
    );
    let ExplicitFields::Struct { children } = fallback else {
        panic!("resource authoring")
    };
    let ExplicitFields::List { element } = &children["key"] else {
        panic!("missing evidence requires field union")
    };
    assert!(
        matches!(element.as_ref(), ExplicitFields::Struct { children } if children.contains_key("reference") && !children.contains_key("provider_default"))
    );
    assert_eq!(
        fixture.calls.lock().unwrap().len(),
        calls_before,
        "writeback must not call the resolver"
    );
}

#[tokio::test]
async fn round2_s3_lists_collect_desired_read_and_saved_references() {
    use crate::schema::{AttributeType, StructField};
    for ordered in [true, false] {
        let mut fixture = Fixture::new(
            string("alias/b"),
            string("alias/c"),
            HashMap::from([
                ("alias/a".into(), resolved("key/a")),
                ("alias/b".into(), resolved("key/a")),
                ("alias/c".into(), resolved("key/a")),
            ]),
            false,
        );
        let element = AttributeType::struct_(
            "EncryptionRule",
            vec![
                StructField::optional(
                    "kms_master_key_id",
                    AttributeType::reference(identity(), None, None),
                )
                .build(),
            ],
        );
        let list_type = if ordered {
            AttributeType::list(element)
        } else {
            AttributeType::unordered_list(element)
        };
        fixture.registry.insert(
            "test",
            crate::schema::ResourceSchema::new("keys.Resource")
                .attribute(AttributeSchema::optional("key", list_type).build())
                .validate()
                .unwrap(),
        );
        let list = |raw: &str| {
            Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                ConcreteValue::Map(indexmap::IndexMap::from([(
                    "kms_master_key_id".into(),
                    string(raw),
                )])),
            )]))
        };
        reset_values(
            &mut fixture,
            list("alias/b"),
            list("alias/c"),
            list("alias/a"),
        );
        let witness = fixture.prepare().await.unwrap();
        assert!(fixture.plan(&witness).unwrap().effects().is_empty());
        let calls = fixture.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0]
                .iter()
                .map(|r| r.value.as_str())
                .collect::<BTreeSet<_>>(),
            BTreeSet::from(["alias/a", "alias/b", "alias/c"])
        );
    }
}

#[tokio::test]
async fn round2_ecr_saved_only_reference_is_collected_against_read_arn() {
    use crate::schema::{AttributeType, StructField};
    for not_found in [false, true] {
        let mut fixture = Fixture::new(
            string("unused"),
            string("unused"),
            HashMap::from([(
                "alias/a".into(),
                if not_found {
                    Ok(wire::ReferenceOutcome::NotFound(error()))
                } else {
                    resolved("key/a")
                },
            )]),
            false,
        );
        fixture.registry.insert(
            "test",
            crate::schema::ResourceSchema::new("keys.Resource")
                .attribute(
                    AttributeSchema::optional(
                        "key",
                        AttributeType::struct_(
                            "EncryptionConfiguration",
                            vec![
                                StructField::optional("encryption_type", AttributeType::string())
                                    .build(),
                                StructField::optional(
                                    "kms_key",
                                    AttributeType::reference(identity(), None, None),
                                )
                                .build(),
                            ],
                        ),
                    )
                    .build(),
                )
                .validate()
                .unwrap(),
        );
        let config = |raw: Option<&str>| {
            let mut fields = indexmap::IndexMap::from([("encryption_type".into(), string("KMS"))]);
            if let Some(raw) = raw {
                fields.insert("kms_key".into(), string(raw));
            }
            Value::Concrete(ConcreteValue::Map(fields))
        };
        reset_values(
            &mut fixture,
            config(None),
            config(Some("key/a")),
            config(Some("alias/a")),
        );
        let witness = fixture.prepare().await.unwrap();
        fixture.plan(&witness).unwrap();
        assert_eq!(fixture.calls.lock().unwrap()[0].len(), 2);
        assert_eq!(
            witness.dangling_paths(&fixture.desired[0].id).count(),
            usize::from(not_found)
        );
    }
}

#[tokio::test]
async fn round2_secret_reference_is_rejected_before_any_guest_call() {
    const SECRET: &str = "alias/private-secret";
    for desired_side in [true, false] {
        for equal in [true, false] {
            for wrapped in [true, false] {
                let secret = if wrapped {
                    Value::Deferred(DeferredValue::Secret(Box::new(string(SECRET))))
                } else {
                    Value::Concrete(ConcreteValue::String(
                        crate::resource::ValueString::diagnostic_sensitive(SECRET),
                    ))
                };
                let ordinary = string(if equal { SECRET } else { "key/a" });
                let (desired, current) = if desired_side {
                    (secret, ordinary)
                } else {
                    (ordinary, secret)
                };
                let fixture = Fixture::new(desired, current, HashMap::new(), false);
                let error = fixture
                    .prepare()
                    .await
                    .expect_err("secret references cannot cross the guest boundary");
                assert!(matches!(
                    error.source,
                    ReferenceFailure::Resolution(ReferenceError::InvalidReference(_))
                ));
                assert!(fixture.calls.lock().unwrap().is_empty());
                assert!(!error.to_string().contains(SECRET));
                assert!(!format!("{error:?}").contains(SECRET));
            }
        }
    }
}

struct ReferenceValidationFactory {
    calls: Arc<Mutex<usize>>,
}
impl ProviderFactory for ReferenceValidationFactory {
    fn name(&self) -> &str {
        "test"
    }
    fn display_name(&self) -> &str {
        "Reference validation fixture"
    }
    fn provider_config_attribute_types(&self) -> crate::schema::ValidatedConfigSchema {
        Default::default()
    }
    fn schemas(&self) -> Vec<ValidatedResourceSchema> {
        vec![
            crate::schema::ResourceSchema::new("keys.Resource")
                .attribute(
                    AttributeSchema::optional(
                        "key",
                        crate::schema::AttributeType::reference(
                            identity(),
                            Some("^alias/[a-z]+$".into()),
                            Some((Some(7), Some(20))),
                        ),
                    )
                    .build(),
                )
                .validate()
                .unwrap(),
        ]
    }
    fn schema_witness(&self) -> crate::schema::ValidatedProviderSchemaWitness {
        crate::schema::ValidatedProviderSchemaWitness::new(
            self.schemas(),
            self.provider_config_attribute_types(),
            Default::default(),
        )
        .unwrap()
    }
    fn validate_config(&self, _: &CanonicalProviderConfigAttributes) -> ProviderResult<()> {
        Ok(())
    }
    fn extract_region(&self, _: &indexmap::IndexMap<String, Value>) -> String {
        "test".into()
    }
    fn create_provider(
        &self,
        _: Option<&str>,
        _: &crate::executor::ProviderReadyConfig,
        _: InitializeOutcomeMint,
    ) -> BoxFuture<'_, ProviderResult<ProviderInitialization>> {
        unimplemented!()
    }
    fn validate_custom_type(
        &self,
        _: &TypeIdentity,
        _: &CanonicalCustomTypeValue,
    ) -> ProviderResult<()> {
        *self.calls.lock().unwrap() += 1;
        Ok(())
    }
}

#[test]
fn round2_reference_validation_is_offline_for_declarations_and_annotations() {
    use crate::parser::{
        CustomTypeResolver, CustomTypeValidationTarget, IdentifiedCustomTypeDeclaration,
        OwnedTypeIdentity,
    };
    let calls = Arc::new(Mutex::new(0));
    let factory = ReferenceValidationFactory {
        calls: calls.clone(),
    };
    let schemas = factory.schemas();
    let factories: Arc<Vec<Box<dyn ProviderFactory>>> = Arc::new(vec![Box::new(factory)]);
    let custom_types = provider_custom_types_from_factories(factories);
    let owned = OwnedTypeIdentity::provider("test", identity());
    custom_types
        .validate(
            CustomTypeValidationTarget::Annotation(&owned),
            &string("alias/valid"),
        )
        .unwrap();
    assert_eq!(
        *calls.lock().unwrap(),
        0,
        "reference annotations only run local string constraints"
    );
    assert!(
        custom_types
            .validate(
                CustomTypeValidationTarget::Annotation(&owned),
                &string("bad")
            )
            .is_err()
    );
    assert_eq!(*calls.lock().unwrap(), 0);
    let attribute = schemas[0].effective_attribute("key").unwrap();
    assert!(
        IdentifiedCustomTypeDeclaration::new(attribute.field_type()).is_none(),
        "resolver identity must not request guest custom-type validation"
    );
    assert!(
        attribute
            .value_type()
            .validate(&string("alias/valid"))
            .is_ok()
    );
    assert!(attribute.value_type().validate(&string("bad")).is_err());
}

#[tokio::test]
async fn round2_provider_instance_batches_run_concurrently() {
    let mut fixture = Fixture::new(string("alias/live"), string("key/a"), HashMap::new(), false);
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    for instance in [None, Some("other".to_string())] {
        fixture.router.add_provider_instance(
            ProviderInstanceKey::new("test", instance.clone()),
            InitializedProvider::unavailable(Box::new(TableProvider {
                table: HashMap::from([("alias/live".into(), resolved("key/a"))]),
                calls: fixture.calls.clone(),
                barrier: Some(barrier.clone()),
                result_count: None,
                fail_batch: false,
                updates: fixture.updates.clone(),
            })),
        );
    }
    let mut other = fixture.desired[0].as_resource().clone();
    other.id.provider_instance = Some("other".into());
    let mut state = State::existing(
        other.id.clone(),
        HashMap::from([("key".into(), string("key/a"))]),
    );
    state.identifier = Some("other-id".into());
    fixture
        .current
        .extend(crate::resource::into_plan_input_current_map(
            HashMap::from([(other.id.clone(), Current::from_state(state))]),
            &fixture.registry,
            &[other.clone()],
        ));
    fixture.desired.extend(
        ResolvedResource::collect_resolved_with_schemas([other], &fixture.registry).unwrap(),
    );
    let witness = tokio::time::timeout(std::time::Duration::from_secs(1), fixture.prepare())
        .await
        .expect("both instance batches must reach the barrier concurrently")
        .unwrap();
    assert!(fixture.plan(&witness).unwrap().effects().is_empty());
    assert_eq!(fixture.calls.lock().unwrap().len(), 2);
}

#[test]
fn reference_plan_errors_preserve_secret_redaction_without_preparation() {
    let value = crate::resource::ValueString::diagnostic_sensitive("alias/private-secret");
    let error = prepare_empty_for_tests()
        .equal(
            &ResourceId::with_identity("keys.Resource", "example"),
            &identity(),
            &value,
            &"key/a".into(),
            &[
                ReferenceOperand {
                    path: "key".into(),
                    side: ReferenceSide::Desired,
                },
                ReferenceOperand {
                    path: "key".into(),
                    side: ReferenceSide::Current,
                },
            ],
        )
        .unwrap_err();
    assert!(matches!(
        error.source,
        ReferenceFailure::Resolution(ReferenceError::InvalidReference(_))
    ));
    assert!(!error.to_string().contains("alias/private-secret"));
    assert!(!format!("{error:?}").contains("alias/private-secret"));
}

#[tokio::test]
async fn round4_byte_equal_current_does_not_turn_saved_pairing_into_a_desired_failure() {
    use crate::schema::AttributeType;
    for list in [false, true] {
        let mut fixture = Fixture::new(
            string("alias/x"),
            string("alias/x"),
            HashMap::from([(
                "alias/x".into(),
                Ok(wire::ReferenceOutcome::NotFound(error())),
            )]),
            false,
        );
        let value = |raw: &str| {
            if list {
                Value::Concrete(ConcreteValue::List(vec![string(raw)]))
            } else {
                string(raw)
            }
        };
        if list {
            fixture.registry.insert(
                "test",
                crate::schema::ResourceSchema::new("keys.Resource")
                    .attribute(
                        AttributeSchema::optional(
                            "key",
                            AttributeType::unordered_list(AttributeType::reference(
                                identity(),
                                None,
                                None,
                            )),
                        )
                        .build(),
                    )
                    .validate()
                    .unwrap(),
            );
        }
        reset_values(
            &mut fixture,
            value("alias/x"),
            value("alias/x"),
            value("alias/y"),
        );
        let references = fixture
            .prepare()
            .await
            .expect("saved pairing cannot invalidate a byte-equal desired/current value");
        assert!(fixture.plan(&references).unwrap().effects().is_empty());
        if !list {
            assert!(
                fixture.calls.lock().unwrap().is_empty(),
                "scalar saved values are not comparison operands when current is present"
            );
        }
    }
}

#[tokio::test]
async fn round8_all_list_references_are_resolved_but_only_compared_failures_matter() {
    use crate::schema::AttributeType;
    for ordered in [false, true] {
        let mut fixture = Fixture::new(
            string("unused"),
            string("unused"),
            HashMap::from([
                (
                    "alias/deleted".into(),
                    Err(wire::ReferenceError::AccessDenied(error())),
                ),
                ("alias/new".into(), resolved("key/new")),
            ]),
            false,
        );
        let element = AttributeType::reference(identity(), None, None);
        fixture.registry.insert(
            "test",
            crate::schema::ResourceSchema::new("keys.Resource")
                .attribute(
                    AttributeSchema::optional(
                        "key",
                        if ordered {
                            AttributeType::list(element)
                        } else {
                            AttributeType::unordered_list(element)
                        },
                    )
                    .build(),
                )
                .validate()
                .unwrap(),
        );
        let list = |raw: &str| {
            Value::Concrete(ConcreteValue::List(vec![
                string("alias/deleted"),
                string(raw),
            ]))
        };
        reset_values(
            &mut fixture,
            list("alias/new"),
            list("key/old"),
            list("key/old"),
        );
        let references = fixture.prepare().await.unwrap();
        assert!(!fixture.plan(&references).unwrap().effects().is_empty());
        assert_eq!(
            fixture.calls.lock().unwrap()[0]
                .iter()
                .map(|request| request.value.as_str())
                .collect::<BTreeSet<_>>(),
            BTreeSet::from(["alias/deleted", "alias/new", "key/old"])
        );
    }
}

#[tokio::test]
async fn round4_saved_deleted_alias_does_not_annotate_a_tag_update() {
    use crate::schema::AttributeType;
    let mut fixture = Fixture::new(
        string("key/a"),
        string("key/a"),
        HashMap::from([(
            "alias/deleted".into(),
            Ok(wire::ReferenceOutcome::NotFound(error())),
        )]),
        false,
    );
    fixture.registry.insert(
        "test",
        crate::schema::ResourceSchema::new("keys.Resource")
            .attribute(
                AttributeSchema::optional("key", AttributeType::reference(identity(), None, None))
                    .build(),
            )
            .attribute(
                AttributeSchema::optional("tags", AttributeType::map(AttributeType::string()))
                    .build(),
            )
            .validate()
            .unwrap(),
    );
    reset_values(
        &mut fixture,
        string("key/a"),
        string("key/a"),
        string("alias/deleted"),
    );
    let mut target = fixture.desired[0].as_resource().clone();
    target.attributes.insert(
        "tags".into(),
        Value::Concrete(ConcreteValue::Map(indexmap::IndexMap::from([(
            "env".into(),
            string("new"),
        )]))),
    );
    fixture.desired =
        ResolvedResource::collect_resolved_with_schemas([target], &fixture.registry).unwrap();
    let references = fixture.prepare().await.unwrap();
    let plan = fixture.plan(&references).unwrap();
    let historical = crate::detail_rows::HistoricalDisplayAttributes::for_plan(&plan);
    let rows = crate::detail_rows::build_detail_rows_by_instance(
        &plan.effects()[0],
        Some(&fixture.registry),
        crate::detail_rows::DetailLevel::Full,
        &historical,
        None,
        &fixture.router,
        crate::detail_rows::DetailComparisons::Planning(&references),
    )
    .unwrap();
    assert!(
        !format!("{rows:?}").contains("reference no longer resolves"),
        "{rows:?}"
    );
}

#[tokio::test]
async fn round4_historical_list_pairing_uses_only_unmatched_reference_values() {
    use crate::schema::{AttributeType, StructField};
    let mut fixture = Fixture::new(
        string("unused"),
        string("unused"),
        HashMap::from([
            (
                "alias/a".into(),
                Err(wire::ReferenceError::AccessDenied(error())),
            ),
            (
                "alias/b".into(),
                Err(wire::ReferenceError::AccessDenied(error())),
            ),
        ]),
        false,
    );
    fixture.registry.insert(
        "test",
        crate::schema::ResourceSchema::new("keys.Resource")
            .attribute(
                AttributeSchema::optional(
                    "key",
                    AttributeType::unordered_list(AttributeType::struct_(
                        "Entry",
                        vec![
                            StructField::optional(
                                "reference",
                                AttributeType::reference(identity(), None, None),
                            )
                            .build(),
                            StructField::optional("mode", AttributeType::string()).build(),
                        ],
                    )),
                )
                .build(),
            )
            .attribute(
                AttributeSchema::optional("tags", AttributeType::map(AttributeType::string()))
                    .build(),
            )
            .validate()
            .unwrap(),
    );
    let list = |reverse: bool, mode: &str| {
        let mut values = vec![
            Value::Concrete(ConcreteValue::Map(indexmap::IndexMap::from([(
                "reference".into(),
                string("alias/a"),
            )]))),
            Value::Concrete(ConcreteValue::Map(indexmap::IndexMap::from([(
                "reference".into(),
                string("alias/b"),
            )]))),
            Value::Concrete(ConcreteValue::Map(indexmap::IndexMap::from([(
                "mode".into(),
                string(mode),
            )]))),
        ];
        if reverse {
            values.reverse();
        }
        Value::Concrete(ConcreteValue::List(values))
    };
    reset_values(
        &mut fixture,
        list(false, "new"),
        list(true, "old"),
        list(true, "old"),
    );
    let mut target = fixture.desired[0].as_resource().clone();
    target.attributes.insert(
        "tags".into(),
        Value::Concrete(ConcreteValue::Map(indexmap::IndexMap::from([(
            "env".into(),
            string("new"),
        )]))),
    );
    fixture.desired =
        ResolvedResource::collect_resolved_with_schemas([target], &fixture.registry).unwrap();
    let references = fixture.prepare().await.unwrap();
    let plan = fixture.plan(&references).unwrap();
    assert!(fixture.calls.lock().unwrap().is_empty());
    let persisted = PersistedAttributes::from_typed_persisted_wire_values(
        fixture.saved.get(&fixture.desired[0].id).unwrap().clone(),
    )
    .unwrap();
    let Current::Present(current) = fixture.current[&fixture.desired[0].id].current() else {
        unreachable!()
    };
    let mut historical = crate::detail_rows::HistoricalDisplayAttributes::for_plan(&plan);
    historical.set_update_from_provider_and_persisted(
        fixture.desired[0].id.clone(),
        &current.state().attributes,
        Some(&persisted),
    );
    let explicit = HashMap::from([(
        fixture.desired[0].id.clone(),
        crate::explicit::build_from_resource(fixture.desired[0].as_resource()),
    )]);
    let rows = crate::detail_rows::build_detail_rows_by_instance(
        &plan.effects()[0],
        Some(&fixture.registry),
        crate::detail_rows::DetailLevel::Full,
        &historical,
        Some(&explicit),
        &fixture.router,
        crate::detail_rows::DetailComparisons::Planning(&references),
    )
    .expect("historical pairing must pre-pair echoed aliases before probing other elements");
    assert!(format!("{rows:?}").contains("new"));
    assert!(fixture.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn round4_reference_depth_limit_is_a_typed_error_with_the_leaf_path() {
    use crate::schema::AttributeType;
    let mut ty = AttributeType::reference(identity(), None, None);
    let mut desired = string("alias/live");
    let mut current = string("key/a");
    for _ in 0..64 {
        ty = AttributeType::list(ty);
        desired = Value::Concrete(ConcreteValue::List(vec![desired]));
        current = Value::Concrete(ConcreteValue::List(vec![current]));
    }
    let mut fixture = Fixture::new(string("unused"), string("unused"), HashMap::new(), false);
    fixture.registry.insert(
        "test",
        crate::schema::ResourceSchema::new("keys.Resource")
            .attribute(AttributeSchema::optional("key", ty).build())
            .validate()
            .unwrap(),
    );
    reset_values(&mut fixture, desired, current.clone(), current);
    let error = fixture
        .prepare()
        .await
        .expect_err("reference recursion may not silently drop leaves");
    assert!(matches!(
        error.source,
        ReferenceFailure::Resolution(ReferenceError::InvalidReference(_))
    ));
    assert_eq!(error.path, format!("key{}", "[0]".repeat(64)));
    assert!(fixture.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn round4_dangling_note_requires_a_rendered_reference_field() {
    use crate::schema::{AttributeType, StructField};
    let mut fixture = Fixture::new(
        string("unused"),
        string("unused"),
        HashMap::from([(
            "alias/deleted".into(),
            Ok(wire::ReferenceOutcome::NotFound(error())),
        )]),
        false,
    );
    fixture.registry.insert(
        "test",
        crate::schema::ResourceSchema::new("keys.Resource")
            .attribute(
                AttributeSchema::optional(
                    "key",
                    AttributeType::unordered_list(AttributeType::struct_(
                        "Entry",
                        vec![
                            StructField::optional(
                                "reference",
                                AttributeType::reference(identity(), None, None),
                            )
                            .build(),
                            StructField::optional("mode", AttributeType::string()).build(),
                        ],
                    )),
                )
                .build(),
            )
            .validate()
            .unwrap(),
    );
    let list = |raw: &str, mode: &str| {
        Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
            ConcreteValue::Map(indexmap::IndexMap::from([
                ("reference".into(), string(raw)),
                ("mode".into(), string(mode)),
            ])),
        )]))
    };
    reset_values(
        &mut fixture,
        list("key/a", "new"),
        list("key/a", "old"),
        list("alias/deleted", "old"),
    );
    let references = fixture.prepare().await.unwrap();
    assert!(
        references
            .dangling_paths(&fixture.desired[0].id)
            .next()
            .is_some()
    );
    let plan = fixture.plan(&references).unwrap();
    let historical = crate::detail_rows::HistoricalDisplayAttributes::for_plan(&plan);
    let rows = crate::detail_rows::build_detail_rows_by_instance(
        &plan.effects()[0],
        Some(&fixture.registry),
        crate::detail_rows::DetailLevel::Full,
        &historical,
        None,
        &fixture.router,
        crate::detail_rows::DetailComparisons::Planning(&references),
    )
    .unwrap();
    assert!(
        !format!("{rows:?}").contains("reference no longer resolves"),
        "{rows:?}"
    );
}

#[tokio::test]
async fn round4_dangling_saved_value_is_labeled_saved_in_its_diff_row() {
    use crate::schema::{AttributeType, StructField};
    let mut fixture = Fixture::new(
        string("unused"),
        string("unused"),
        HashMap::from([(
            "alias/deleted".into(),
            Ok(wire::ReferenceOutcome::NotFound(error())),
        )]),
        false,
    );
    fixture.registry.insert(
        "test",
        crate::schema::ResourceSchema::new("keys.Resource")
            .attribute(
                AttributeSchema::optional(
                    "key",
                    AttributeType::struct_(
                        "Encryption",
                        vec![
                            StructField::optional(
                                "reference",
                                AttributeType::reference(identity(), None, None),
                            )
                            .build(),
                            StructField::optional("mode", AttributeType::string()).build(),
                        ],
                    ),
                )
                .build(),
            )
            .validate()
            .unwrap(),
    );
    let config = |raw: Option<&str>| {
        let mut fields = indexmap::IndexMap::from([("mode".into(), string("KMS"))]);
        if let Some(raw) = raw {
            fields.insert("reference".into(), string(raw));
        }
        Value::Concrete(ConcreteValue::Map(fields))
    };
    reset_values(
        &mut fixture,
        config(None),
        config(Some("key/a")),
        config(Some("alias/deleted")),
    );
    let references = fixture.prepare().await.unwrap();
    let plan = fixture.plan(&references).unwrap();
    let historical = crate::detail_rows::HistoricalDisplayAttributes::for_plan(&plan);
    let rows = crate::detail_rows::build_detail_rows_by_instance(
        &plan.effects()[0],
        Some(&fixture.registry),
        crate::detail_rows::DetailLevel::Full,
        &historical,
        None,
        &fixture.router,
        crate::detail_rows::DetailComparisons::Planning(&references),
    )
    .unwrap();
    assert!(
        format!("{rows:?}").contains("key.reference: saved reference no longer resolves"),
        "{rows:?}"
    );
}

#[tokio::test]
async fn round4_historical_pairing_does_not_compare_unmanaged_reference_peers() {
    use crate::schema::{AttributeType, StructField};
    let mut fixture = Fixture::new(string("unused"), string("unused"), HashMap::new(), false);
    fixture.registry.insert(
        "test",
        crate::schema::ResourceSchema::new("keys.Resource")
            .attribute(
                AttributeSchema::optional(
                    "key",
                    AttributeType::unordered_list(AttributeType::struct_(
                        "Entry",
                        vec![
                            StructField::optional("id", AttributeType::int()).build(),
                            StructField::optional(
                                "reference",
                                AttributeType::reference(identity(), None, None),
                            )
                            .build(),
                            StructField::optional("mode", AttributeType::string()).build(),
                        ],
                    )),
                )
                .build(),
            )
            .validate()
            .unwrap(),
    );
    let list = |current: bool| {
        Value::Concrete(ConcreteValue::List(
            (0..40)
                .map(|index| {
                    let mut fields = indexmap::IndexMap::from([
                        ("id".into(), Value::Concrete(ConcreteValue::Int(index))),
                        ("mode".into(), string(if current { "old" } else { "new" })),
                    ]);
                    if current {
                        fields.insert("reference".into(), string(&format!("alias/{index}")));
                    }
                    Value::Concrete(ConcreteValue::Map(fields))
                })
                .collect(),
        ))
    };
    reset_values(&mut fixture, list(false), list(true), list(true));
    fixture.saved = RawSavedAttrs::default().lift(&fixture.registry);
    let references = fixture.prepare().await.unwrap();
    let plan = fixture.plan(&references).unwrap();
    assert!(fixture.calls.lock().unwrap().is_empty());
    let historical = crate::detail_rows::HistoricalDisplayAttributes::for_plan(&plan);
    crate::detail_rows::build_detail_rows_by_instance(
        &plan.effects()[0],
        Some(&fixture.registry),
        crate::detail_rows::DetailLevel::Full,
        &historical,
        None,
        &fixture.router,
        crate::detail_rows::DetailComparisons::Planning(&references),
    )
    .expect("unmanaged historical references are not operands for desired/current equality");
    assert!(fixture.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn round4_omitted_write_only_list_reference_is_still_a_desired_operand() {
    use crate::schema::{AttributeType, StructField};
    let mut fixture = Fixture::new(
        string("unused"),
        string("unused"),
        HashMap::from([(
            "alias/deleted".into(),
            Ok(wire::ReferenceOutcome::NotFound(error())),
        )]),
        false,
    );
    let schema = crate::schema::ResourceSchema::new("keys.Resource")
        .attribute(
            AttributeSchema::optional(
                "key",
                AttributeType::unordered_list(AttributeType::struct_(
                    "Entry",
                    vec![
                        StructField::optional(
                            "reference",
                            AttributeType::reference(identity(), None, None),
                        )
                        .write_only()
                        .build(),
                    ],
                )),
            )
            .build(),
        )
        .validate()
        .unwrap();
    let list = |raw: &str| {
        Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
            ConcreteValue::Map(indexmap::IndexMap::from([(
                "reference".into(),
                string(raw),
            )])),
        )]))
    };
    let accepted = indexmap::IndexMap::from([("key".into(), list("key/a"))]);
    let mut record = NestedWriteOnlyRecord::new();
    record
        .record_sent(schema.as_schema(), ["key"], &accepted, None)
        .unwrap();
    fixture.registry.insert("test", schema);
    let mut target = fixture.desired[0].as_resource().clone();
    target
        .attributes
        .insert("key".into(), list("alias/deleted"));
    fixture.saved = RawSavedAttrs::from_persisted(
        HashMap::from([(
            target.id.clone(),
            PersistedAttributes::from_typed_persisted_wire_values(accepted.into_iter().collect())
                .unwrap(),
        )]),
        HashMap::from([(target.id.clone(), record)]),
    )
    .lift(&fixture.registry);
    fixture.current = crate::resource::into_plan_input_current_map(
        HashMap::from([(
            target.id.clone(),
            Current::from_state(
                State::existing(target.id.clone(), HashMap::new()).with_identifier("resource-123"),
            ),
        )]),
        &fixture.registry,
        &[target.clone()],
    );
    fixture.desired =
        ResolvedResource::collect_resolved_with_schemas([target], &fixture.registry).unwrap();
    let error = fixture
        .prepare()
        .await
        .expect_err("a write-only desired lookup cannot use the saved-only failure rule");
    assert!(
        matches!(error.source, ReferenceFailure::DesiredNotFound(_)),
        "{error:?}"
    );
}

#[tokio::test]
async fn round5_provisional_errors_and_decisions_are_discarded_until_a_clean_run() {
    let fixture = Fixture::new(
        string("alias/live"),
        string("key/a"),
        HashMap::from([("alias/live".into(), resolved("key/a"))]),
        false,
    );
    let mut passes = 0;
    let prepared = ReferencePreparation::default()
        .run(
            &fixture.router,
            || fixture.reference_values(),
            |session| {
                passes += 1;
                let result = session
                    .references
                    .equal(
                        &fixture.desired[0].id,
                        &identity(),
                        &"alias/live".into(),
                        &"key/a".into(),
                        &[
                            ReferenceOperand {
                                path: "key".into(),
                                side: ReferenceSide::Desired,
                            },
                            ReferenceOperand {
                                path: "key".into(),
                                side: ReferenceSide::Current,
                            },
                        ],
                    )
                    .map_err(crate::differ::PlanComparisonError::from)
                    .and_then(|equal| {
                        if equal {
                            Ok("clean")
                        } else {
                            Err(ProviderError::invalid_input(
                                "provisional decision must never escape",
                            )
                            .into())
                        }
                    });
                session.result(result)
            },
        )
        .await
        .unwrap();
    assert_eq!(passes, 2);
    assert_eq!(prepared.value, "clean");
    assert!(prepared.references.recording.is_none());
    assert_eq!(fixture.calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn round5_dangling_saved_leaf_does_not_annotate_a_different_list_element() {
    let mut fixture = round5_list_fixture(false, false);
    fixture.router.add_provider(
        "test".into(),
        InitializedProvider::unavailable(Box::new(TableProvider {
            table: HashMap::from([(
                "alias/deleted".into(),
                Ok(wire::ReferenceOutcome::NotFound(error())),
            )]),
            calls: fixture.calls.clone(),
            barrier: None,
            result_count: None,
            fail_batch: false,
            updates: fixture.updates.clone(),
        })),
    );
    let list = |first: &str, second: &str| {
        Value::Concrete(ConcreteValue::List(vec![
            round5_entry(first, "first", string("same")),
            round5_entry(second, "second", string("same")),
        ]))
    };
    reset_values(
        &mut fixture,
        list("key/a", "key/new"),
        list("key/a", "key/old"),
        list("alias/deleted", "key/old"),
    );
    let references = fixture.prepare().await.unwrap();
    assert!(
        references
            .dangling_references(&fixture.desired[0].id)
            .any(|reference| reference.side == ReferenceSide::Saved
                && reference.path == "key[0].kms")
    );
    let plan = fixture.plan(&references).unwrap();
    let rows = crate::detail_rows::build_detail_rows_by_instance(
        &plan.effects()[0],
        Some(&fixture.registry),
        crate::detail_rows::DetailLevel::Full,
        &crate::detail_rows::HistoricalDisplayAttributes::for_plan(&plan),
        None,
        &fixture.router,
        crate::detail_rows::DetailComparisons::Planning(&references),
    )
    .unwrap();
    assert!(format!("{rows:?}").contains("key/new"));
    assert!(
        !format!("{rows:?}").contains("reference no longer resolves"),
        "{rows:?}"
    );
}

#[tokio::test]
async fn round5_current_dangling_notes_keep_the_original_string_list_index() {
    use crate::schema::AttributeType;
    for first in ["key/same", "alias/deleted"] {
        let mut fixture = Fixture::new(
            string("unused"),
            string("unused"),
            HashMap::from([(
                "alias/deleted".into(),
                Ok(wire::ReferenceOutcome::NotFound(error())),
            )]),
            false,
        );
        fixture.registry.insert(
            "test",
            crate::schema::ResourceSchema::new("keys.Resource")
                .attribute(
                    AttributeSchema::optional(
                        "key",
                        AttributeType::unordered_list(AttributeType::reference(
                            identity(),
                            None,
                            None,
                        )),
                    )
                    .build(),
                )
                .validate()
                .unwrap(),
        );
        let list = |second: &str| {
            Value::Concrete(ConcreteValue::StringList(vec![first.into(), second.into()]))
        };
        reset_values(
            &mut fixture,
            list("key/new"),
            list("alias/deleted"),
            list("alias/deleted"),
        );
        fixture.saved = RawSavedAttrs::default().lift(&fixture.registry);
        let references = fixture.prepare().await.unwrap();
        let plan = fixture.plan(&references).unwrap();
        let rows = crate::detail_rows::build_detail_rows_by_instance(
            &plan.effects()[0],
            Some(&fixture.registry),
            crate::detail_rows::DetailLevel::Full,
            &crate::detail_rows::HistoricalDisplayAttributes::for_plan(&plan),
            None,
            &fixture.router,
            crate::detail_rows::DetailComparisons::Planning(&references),
        )
        .unwrap();
        let rows = format!("{rows:?}");
        assert_eq!(
            rows.contains("key[1]: current reference no longer resolves"),
            first == "key/same",
            "{rows}"
        );
        assert!(
            !rows.contains("key[0]: current reference no longer resolves"),
            "{rows}"
        );
        // The renderer uses set semantics for StringList rows: a duplicate
        // deleted alias retained as unchanged must not get a dangling note.
    }
}

#[tokio::test]
async fn round5_projected_list_notes_keep_the_current_operand_origin() {
    let mut fixture = round5_list_fixture(false, false);
    fixture.router.add_provider(
        "test".into(),
        InitializedProvider::unavailable(Box::new(TableProvider {
            table: HashMap::from([(
                "alias/deleted".into(),
                Ok(wire::ReferenceOutcome::NotFound(error())),
            )]),
            calls: fixture.calls.clone(),
            barrier: None,
            result_count: None,
            fail_batch: false,
            updates: fixture.updates.clone(),
        })),
    );
    let desired = Value::Concrete(ConcreteValue::List(vec![round5_entry(
        "key/new",
        "user",
        string("same"),
    )]));
    let current = Value::Concrete(ConcreteValue::List(vec![
        Value::Concrete(ConcreteValue::Map(indexmap::IndexMap::from([(
            "x".into(),
            string("server"),
        )]))),
        round5_entry("alias/deleted", "user", string("same")),
    ]));
    reset_values(&mut fixture, desired, current.clone(), current);
    let explicit = HashMap::from([(
        fixture.desired[0].id.clone(),
        crate::explicit::ExplicitFields::Struct {
            children: HashMap::from([(
                "key".into(),
                crate::explicit::ExplicitFields::ListElements {
                    elements: vec![
                        crate::explicit::ExplicitFields::Unrecorded,
                        crate::explicit::build_from_value(&round5_entry(
                            "alias/deleted",
                            "user",
                            string("same"),
                        )),
                    ],
                },
            )]),
        },
    )]);
    let prepared = ReferencePreparation::default()
        .plan(
            &fixture.desired,
            &[],
            &fixture.router,
            crate::differ::PlanPriorStates::new(&fixture.current, &crate::differ::Orphans::new()),
            &HashMap::new(),
            &fixture.registry,
            &fixture.saved,
            &explicit,
            &[],
        )
        .await
        .unwrap();
    let plan = prepared.value;
    let rows = crate::detail_rows::build_detail_rows_by_instance(
        &plan.effects()[0],
        Some(&fixture.registry),
        crate::detail_rows::DetailLevel::Full,
        &crate::detail_rows::HistoricalDisplayAttributes::for_planning(&plan, &fixture.saved),
        Some(&explicit),
        &fixture.router,
        crate::detail_rows::DetailComparisons::Planning(&prepared.references),
    )
    .unwrap();
    let rows = format!("{rows:?}");
    assert!(
        rows.contains("key[1].kms: current reference no longer resolves"),
        "{rows}"
    );
    assert!(
        !rows.contains("key[0].kms: current reference no longer resolves"),
        "{rows}"
    );
}

#[tokio::test]
async fn round5_saved_map_union_grafts_retain_the_saved_side() {
    use crate::schema::AttributeType;
    let mut fixture = Fixture::new(
        string("unused"),
        string("unused"),
        HashMap::from([(
            "alias/deleted".into(),
            Ok(wire::ReferenceOutcome::NotFound(error())),
        )]),
        false,
    );
    fixture.registry.insert(
        "test",
        crate::schema::ResourceSchema::new("keys.Resource")
            .attribute(
                AttributeSchema::optional(
                    "key",
                    AttributeType::union(vec![
                        AttributeType::string(),
                        AttributeType::map(AttributeType::reference(identity(), None, None)),
                    ]),
                )
                .build(),
            )
            .validate()
            .unwrap(),
    );
    let map = |fallback: Option<&str>| {
        let mut fields = indexmap::IndexMap::from([("owned".into(), string("key/owned"))]);
        if let Some(fallback) = fallback {
            fields.insert("fallback".into(), string(fallback));
        }
        Value::Concrete(ConcreteValue::Map(fields))
    };
    reset_values(
        &mut fixture,
        map(None),
        map(Some("key/a")),
        map(Some("alias/deleted")),
    );
    let references = fixture.prepare().await.unwrap();
    let plan = fixture.plan(&references).unwrap();
    let rows = crate::detail_rows::build_detail_rows_by_instance(
        &plan.effects()[0],
        Some(&fixture.registry),
        crate::detail_rows::DetailLevel::Full,
        &crate::detail_rows::HistoricalDisplayAttributes::for_plan(&plan),
        None,
        &fixture.router,
        crate::detail_rows::DetailComparisons::Planning(&references),
    )
    .unwrap();
    assert!(
        format!("{rows:?}").contains("key['fallback']: saved reference no longer resolves"),
        "{rows:?}"
    );
}

#[tokio::test]
async fn round5_projected_write_only_reference_is_still_a_desired_operand() {
    use crate::schema::{AttributeType, StructField};
    let mut fixture = Fixture::new(
        string("unused"),
        string("unused"),
        HashMap::from([(
            "alias/deleted".into(),
            Ok(wire::ReferenceOutcome::NotFound(error())),
        )]),
        false,
    );
    let schema = crate::schema::ResourceSchema::new("keys.Resource")
        .attribute(
            AttributeSchema::optional(
                "key",
                AttributeType::struct_(
                    "Encryption",
                    vec![
                        StructField::optional(
                            "kms",
                            AttributeType::reference(identity(), None, None),
                        )
                        .write_only()
                        .build(),
                        StructField::optional("mode", AttributeType::string()).build(),
                    ],
                ),
            )
            .build(),
        )
        .validate()
        .unwrap();
    let config = |kms: Option<&str>| {
        let mut values = indexmap::IndexMap::from([("mode".into(), string("same"))]);
        if let Some(kms) = kms {
            values.insert("kms".into(), string(kms));
        }
        Value::Concrete(ConcreteValue::Map(values))
    };
    let accepted = indexmap::IndexMap::from([("key".into(), config(Some("key/old")))]);
    let mut record = NestedWriteOnlyRecord::new();
    record
        .record_sent(schema.as_schema(), ["key"], &accepted, None)
        .unwrap();
    fixture.registry.insert("test", schema);
    reset_values(
        &mut fixture,
        config(Some("alias/deleted")),
        config(None),
        config(Some("key/old")),
    );
    let id = fixture.desired[0].id.clone();
    fixture.saved = RawSavedAttrs::from_persisted(
        HashMap::from([(
            id.clone(),
            PersistedAttributes::from_typed_persisted_wire_values(accepted.into_iter().collect())
                .unwrap(),
        )]),
        HashMap::from([(id.clone(), record)]),
    )
    .lift(&fixture.registry);
    let explicit = HashMap::from([(
        id,
        crate::explicit::build_from_resource(fixture.desired[0].as_resource()),
    )]);
    let error = ReferencePreparation::default()
        .plan(
            &fixture.desired,
            &[],
            &fixture.router,
            crate::differ::PlanPriorStates::new(&fixture.current, &crate::differ::Orphans::new()),
            &HashMap::new(),
            &fixture.registry,
            &fixture.saved,
            &explicit,
            &[],
        )
        .await
        .unwrap_err();
    let crate::differ::PlanComparisonError::Reference(error) = error else {
        panic!("expected a typed reference error")
    };
    assert!(
        matches!(error.source, ReferenceFailure::DesiredNotFound(_)),
        "{error:?}"
    );
    assert_eq!(error.path, "key.kms");
}
