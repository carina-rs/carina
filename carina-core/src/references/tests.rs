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
            Ok(requests
                .iter()
                .map(|request| {
                    match self.table.get(&request.value).cloned().unwrap_or_else(|| {
                        Ok(wire::ReferenceOutcome::Resolved(request.value.clone()))
                    }) {
                        Ok(outcome) => ReferenceOutcome::from_guest(outcome),
                        Err(wire::ReferenceError::AccessDenied(error)) => Err(
                            ReferenceError::AccessDenied(ProviderError::from_wire(*error)),
                        ),
                        Err(wire::ReferenceError::Transient(error)) => {
                            Err(ReferenceError::Transient(ProviderError::from_wire(*error)))
                        }
                        Err(wire::ReferenceError::InvalidReference(error)) => Err(
                            ReferenceError::InvalidReference(ProviderError::from_wire(*error)),
                        ),
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
        _: &ResourceId,
        _: &AppliedIdentifier,
        _: UpdateRequest,
    ) -> BoxFuture<'_, ProviderResult<UpdateOutcome>> {
        unimplemented!()
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
}
impl Fixture {
    fn new(
        desired: Value,
        current: Value,
        table: HashMap<String, TableResult>,
        create_only: bool,
    ) -> Self {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut router = ProviderRouter::new();
        router.add_provider(
            "test".into(),
            InitializedProvider::unavailable(Box::new(TableProvider {
                table,
                calls: calls.clone(),
                barrier: None,
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
        }
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
        crate::differ::create_plan(
            &self.desired,
            &[],
            &self.router,
            crate::differ::PlanPriorStates::new(&self.current, &crate::differ::Orphans::new()),
            &HashMap::new(),
            &self.registry,
            &self.saved,
            &HashMap::new(),
            &[],
            references,
        )
    }
}
fn resolved(value: &str) -> TableResult {
    Ok(wire::ReferenceOutcome::Resolved(value.into()))
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
                    assert!(kind != "resolved" && (desired_side || kind != "not_found"));
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
    assert_eq!(source.path, identity().to_string());
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
                &"key/a".into()
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
    assert_eq!(fixture.calls.lock().unwrap()[0].len(), 2);
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
        )
        .unwrap_err();
    assert!(matches!(
        error.source,
        ReferenceFailure::Resolution(ReferenceError::InvalidReference(_))
    ));
    assert!(!error.to_string().contains("alias/private-secret"));
    assert!(!format!("{error:?}").contains("alias/private-secret"));
}
