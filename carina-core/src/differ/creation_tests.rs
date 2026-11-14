use super::*;
use crate::effect::TemporaryName;
use crate::input_rules::{InputCheckedPlan, InputWriteContext};
use crate::provider::{AppliedIdentifier, CreateAttempt, InFlightCreate, PendingToken, Sent};
use crate::resource::{DeferredValue, LoopSlot};
use crate::schema::{AttributeSchema, InputPath, InputPredicate, InputRule, InputRuleCode};

fn schemas() -> SchemaRegistry {
    let mut registry = SchemaRegistry::new();
    registry.insert(
        "mock",
        crate::schema::ResourceSchema::new("Bucket")
            .with_coexisting_replacement()
            .attribute(
                AttributeSchema::optional("name", crate::schema::AttributeType::string()).build(),
            )
            .input_rule(InputRule::value(
                crate::schema::InputRuleScope::CreateOnly,
                InputPath::new("name").unwrap(),
                InputRuleCode::new("bucket.not_ipv4").unwrap(),
                InputPredicate::NotIpv4,
            ))
            .validate()
            .unwrap(),
    );
    registry
}

fn resource(value: Option<Value>) -> ResolvedResource {
    let mut resource = Resource::with_provider("mock", "Bucket", "legacy", None);
    if let Some(value) = value {
        resource = resource.with_attribute("name", value);
    }
    ResolvedResource::resolve_raw_fixture(resource, &schemas()).unwrap()
}

fn string(value: &str) -> Value {
    Value::Concrete(ConcreteValue::String(value.into()))
}
fn unknown() -> Value {
    Value::Deferred(crate::resource::DeferredValue::Unknown(
        crate::resource::ApplyTimeUnknown::PendingReRead(
            crate::resource::PendingReRead::not_found(),
        ),
    ))
}

fn legacy() -> ResolvedResource {
    resource(Some(string("192.168.1.1")))
}

fn checked(
    plan: Plan,
    current: &HashMap<ResourceId, Current>,
) -> Result<InputCheckedPlan, crate::input_rules::InputAdmissionFailure> {
    PendingCascadePlan {
        registry: schemas(),
        plan,
        deferred: ResolvedDeferredCreateTargets::default(),
    }
    .finish(std::iter::empty(), |_| {})
    .check_inputs(&schemas(), current)
}

fn delete(resource: &ResolvedResource) -> DeferredReplaceDelete {
    DeferredReplaceDelete {
        generation: EffectGeneration::Current,
        previous_attributes: HashMap::new(),
        blocked_by_updates: Default::default(),
        id: resource.resolved_id().clone(),
        identifier: AppliedIdentifier::new("old-id").unwrap(),
        directives: Default::default(),
        binding: None,
        dependencies: Default::default(),
        explicit_dependencies: Default::default(),
    }
}

fn replacement(create_before_destroy: bool, create: ResolvedResource) -> Plan {
    let mut plan = Plan::new();
    add_replacement(&mut plan, create_before_destroy, create);
    plan
}

fn add_replacement(plan: &mut Plan, create_before_destroy: bool, create: ResolvedResource) {
    plan.add_replacement(ReplacementGroup {
        replacement: crate::test_support::checked_replacement(
            create.clone(),
            delete(&create),
            create_before_destroy,
        ),
        changed_create_only: ChangedCreateOnly::new(vec!["generation".into()]).unwrap(),
        cascade_ref_hints: Vec::new(),
        consumer_updates: Default::default(),
    });
}

#[test]
fn creation_admission_rejects_known_create_without_leaking_input() {
    let mut plan = Plan::new();
    plan.add(Effect::Create(legacy()));
    let rejected = checked(plan, &HashMap::new()).unwrap_err();
    assert_eq!(rejected.errors().len(), 1);
    let error = &rejected.errors()[0];
    assert_eq!(error.resource_id, legacy().id);
    assert!(
        matches!(&error.kind, PlanErrorKind::InputRule { path, code, context: InputWriteContext::Create, .. } if path == "name" && code == "bucket.not_ipv4")
    );
    assert!(!format!("{error:?}").contains("192.168.1.1"));
    assert!(!format!("{rejected:?}").contains("192.168.1.1"));
}

#[test]
fn creation_admission_checks_both_replacement_orders_including_cascades() {
    for cbd in [false, true] {
        let mut plan = replacement(cbd, legacy());
        // Cascades have the same final Create/Delete representation.
        let mut cascade = legacy().as_inner().clone();
        cascade.id = ResourceId::with_provider_identity("mock", "Bucket", "cascade", None);
        let cascade = ResolvedResource::resolve_with_schemas(cascade, &schemas()).unwrap();
        add_replacement(&mut plan, cbd, cascade);
        let rejected = checked(plan, &HashMap::new()).unwrap_err();
        assert_eq!(rejected.errors().len(), 2);
        assert!(rejected.errors().iter().all(|error| matches!(&error.kind, PlanErrorKind::InputRule { context: InputWriteContext::Replacement { create_before_destroy }, .. } if *create_before_destroy == cbd)));
    }
}

#[test]
fn creation_admission_uses_actual_temporary_create_name() {
    let mut plan = replacement(true, legacy());
    plan.replace_display_mut()[0].temporary_name = Some(TemporaryName {
        attribute: "name".into(),
        original_value: "valid-name".into(),
        temporary_value: "192.168.1.1".into(),
        can_rename: false,
    });
    assert!(checked(plan, &HashMap::new()).is_err());
    let mut plan = replacement(true, resource(Some(string("valid-temporary"))));
    plan.replace_display_mut()[0].temporary_name = Some(TemporaryName {
        attribute: "name".into(),
        original_value: "192.168.1.1".into(),
        temporary_value: "valid-temporary".into(),
        can_rename: false,
    });
    assert!(checked(plan, &HashMap::new()).is_ok());
}

#[test]
fn creation_admission_exempts_noncreating_effects_and_adoption() {
    let resource = legacy();
    let mut plan = Plan::new();
    plan.add(Effect::Import {
        id: resource.resolved_id().clone(),
        identifier: string("old-id"),
    });
    plan.add(Effect::Update {
        from: Box::new(
            State::existing(resource.id.clone(), HashMap::new()).with_identifier("old-id"),
        ),
        identifier: AppliedIdentifier::new("old-id").unwrap(),
        to: resource.clone(),
        changed_attributes: vec!["comment".into()],
    });
    let deleted = delete(&resource);
    plan.add(Effect::Delete {
        id: deleted.id,
        identifier: deleted.identifier,
        generation: EffectGeneration::Current,
        directives: deleted.directives,
        binding: deleted.binding,
        dependencies: deleted.dependencies,
        explicit_dependencies: deleted.explicit_dependencies,
        blocked_by_updates: Default::default(),
    });
    plan.add(Effect::Remove {
        id: resource.resolved_id().clone(),
    });
    plan.add(Effect::Move {
        from: resource.resolved_id().clone(),
        to: resource.resolved_id().clone(),
        origin: None,
    });
    plan.add(Effect::Read {
        resource: crate::resource::ResolvedDataSource::collect_resolved([
            crate::resource::DataSource::with_provider("mock", "Bucket", "lookup", None)
                .with_attribute("name", string("192.168.1.1")),
        ])
        .unwrap()
        .remove(0),
    });
    plan.add(Effect::Wait {
        id: ResolvedResourceId::with_provider_identity("mock", "Bucket", "wait", None),
        target_id: crate::wait::ManagedWaitTarget::new(&resource),
        until: crate::wait::predicate::WaitPredicate::equals(
            crate::wait::predicate::AttrPath::single("name"),
            serde_json::from_value::<crate::wait::WaitRhs>(
                serde_json::json!({"String": "'192.168.1.1'"}),
            )
            .unwrap(),
        )
        .unwrap(),
        until_surface: "legacy.name == legacy-name".into(),
        timeout: std::time::Duration::from_secs(1),
        interval: std::time::Duration::from_secs(1),
        explicit_dependencies: Default::default(),
    });
    plan.add(Effect::ReleaseListKeys {
        id: resource.resolved_id(),
        attribute: "settings".into(),
        key_field: crate::schema::FieldName::declared("key"),
        keys: crate::effect::ReleasedListKeys::try_new(vec![crate::effect::ReleasedListKey::new(
            "entry",
            string("192.168.1.1"),
        )])
        .unwrap(),
    });
    assert!(
        checked(plan, &HashMap::new())
            .unwrap()
            .pending_inputs()
            .is_empty()
    );
    // Adoption/no-op has no creating effect, even with legacy values in state.
    let current = Current::from_state(
        crate::resource::State::existing(
            resource.id.clone(),
            HashMap::from([("name".into(), string("192.168.1.1"))]),
        )
        .with_identifier("old-id"),
    );
    assert!(
        checked(
            Plan::new(),
            &HashMap::from([(resource.id.clone(), current)])
        )
        .is_ok()
    );
}

#[test]
fn creation_admission_classifies_resume_from_identifier_presence() {
    for issued in [false, true] {
        let resource = legacy();
        let attempt = CreateAttempt::new(resource.id.clone()).0;
        let pending = if issued {
            attempt
                .issued(
                    AppliedIdentifier::new("old-id").unwrap(),
                    Sent::AllRequestAttributes,
                )
                .pending(PendingToken::new("token"), 1)
        } else {
            attempt.pending(PendingToken::new("token"), Sent::AllRequestAttributes, 1)
        };
        let pending = InFlightCreate::from_pending(pending);
        let mut plan = Plan::new();
        plan.add_resume_create(resource.clone(), pending.started_at().clone());
        let current = HashMap::from([(resource.id.clone(), Current::create_in_flight(pending))]);
        assert_eq!(checked(plan, &current).is_ok(), issued);
    }
}

#[test]
fn creation_admission_optional_absence_is_exempt_but_unknown_is_pending() {
    for (value, pending) in [(None, false), (Some(unknown()), true)] {
        let mut plan = Plan::new();
        plan.add(Effect::Create(resource(value)));
        let checked = checked(plan, &HashMap::new()).unwrap();
        assert_eq!(!checked.pending_inputs().is_empty(), pending);
    }
}

#[test]
fn creation_admission_deferred_template_retains_exact_slot_obligation() {
    let payload = deferred_payload();
    let mut plan = Plan::new();
    plan.add(Effect::DeferredCreate(payload));
    let checked = checked(plan, &HashMap::new()).unwrap();
    assert_eq!(checked.pending_inputs().len(), 1);
    assert_eq!(checked.pending_inputs()[0].target().effect_index(), 0);
}

#[test]
fn creation_admission_deferred_template_literal_name_leaves_no_obligation() {
    let mut payload = deferred_payload();
    payload.template.template_attributes_mut().insert(
        "name".into(),
        Value::Concrete(ConcreteValue::String("valid-template".into())),
    );
    let mut plan = Plan::new();
    plan.add(Effect::DeferredCreate(payload));
    let checked = checked(plan, &HashMap::new()).unwrap();
    assert!(checked.pending_inputs().is_empty());
}

fn deferred_payload() -> DeferredCreatePayload {
    let template: crate::parser::DeferredForExpression = crate::parser::DeferredForExpression::new(
        None,
        1,
        "for item in parent.items".into(),
        crate::parser::DeferredForTarget::bound(
            crate::resource::absolute_binding_address_for_test("children"),
        )
        .unwrap(),
        Value::root_resource_ref("parent", "items", vec![]),
        crate::parser::ForBinding::Simple("item".into()),
        Resource::with_provider("mock", "Bucket", "children", None).with_attribute(
            "name",
            // The loop's item has no known element type.
            Value::Deferred(DeferredValue::Placeholder(LoopSlot::Value)),
        ),
    );
    crate::test_support::compiled_loop(Box::new(template))
}

#[test]
fn creation_admission_deferred_replace_retains_all_absorbed_delete_slots() {
    let DeferredCreatePayload {
        template,
        dependency_template,
    } = deferred_payload();
    let mut plan = Plan::new();
    let deletes = ["old-a", "old-b"].map(|identity| DeferredReplaceDelete {
        generation: EffectGeneration::Current,
        previous_attributes: HashMap::new(),
        id: ResolvedResourceId::with_provider_identity("mock", "Bucket", identity, None),
        identifier: AppliedIdentifier::new(identity).unwrap(),
        directives: Default::default(),
        binding: None,
        dependencies: Default::default(),
        explicit_dependencies: Default::default(),
        blocked_by_updates: Default::default(),
    });
    plan.add(Effect::DeferredReplace(Box::new(
        DeferredReplacePayload::new(
            NonEmptyDeletes::try_new(deletes.into()).unwrap(),
            template,
            dependency_template,
        ),
    )));
    let checked = checked(plan, &HashMap::new()).unwrap();
    assert_eq!(checked.pending_inputs().len(), 1);
    let obligation = &checked.pending_inputs()[0];
    assert!(matches!(
        checked.effects()[obligation.target().effect_index()],
        Effect::DeferredReplace(_)
    ));
    assert_eq!(obligation.paired_deletes().len(), 2);
    assert_eq!(
        obligation
            .paired_deletes()
            .iter()
            .map(|delete| (delete.effect_index(), delete.absorbed_index()))
            .collect::<Vec<_>>(),
        vec![(0, Some(0)), (0, Some(1))]
    );
    assert_ne!(
        obligation.paired_deletes()[0].instance(),
        obligation.paired_deletes()[1].instance()
    );
}

#[test]
fn creation_admission_delete_first_pending_is_paired_with_exact_delete() {
    let checked = checked(
        replacement(false, resource(Some(unknown()))),
        &HashMap::new(),
    )
    .unwrap();
    let obligation = &checked.pending_inputs()[0];
    assert_eq!(obligation.target().effect_index(), 1);
    assert_eq!(
        obligation.paired_deletes().first().unwrap().effect_index(),
        0
    );
}

#[test]
fn creation_admission_required_absence_stays_pending() {
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        crate::schema::ResourceSchema::new("Bucket")
            .attribute(
                AttributeSchema::required("name", crate::schema::AttributeType::string()).build(),
            )
            .input_rule(InputRule::value(
                crate::schema::InputRuleScope::CreateOnly,
                InputPath::new("name").unwrap(),
                InputRuleCode::new("bucket.not_ipv4").unwrap(),
                InputPredicate::NotIpv4,
            ))
            .validate()
            .unwrap(),
    );
    let mut plan = Plan::new();
    plan.add(Effect::Create(resource(None)));
    let plan = PendingCascadePlan {
        registry: schemas.clone(),
        plan,
        deferred: ResolvedDeferredCreateTargets::default(),
    }
    .finish(std::iter::empty(), |_| {})
    .check_inputs(&schemas, &HashMap::new())
    .unwrap();
    assert_eq!(plan.pending_inputs().len(), 1);
}

#[test]
fn creation_admission_slots_are_scoped_to_the_exact_finalized_plan() {
    let first = checked(
        replacement(false, resource(Some(unknown()))),
        &HashMap::new(),
    )
    .unwrap();
    let second = checked(
        replacement(false, resource(Some(unknown()))),
        &HashMap::new(),
    )
    .unwrap();
    assert_ne!(
        first.pending_inputs()[0].target(),
        second.pending_inputs()[0].target()
    );
    assert_ne!(
        first.pending_inputs()[0].paired_deletes().first(),
        second.pending_inputs()[0].paired_deletes().first()
    );
    let cloned = first.clone();
    assert_eq!(
        first.pending_inputs()[0].target(),
        cloned.pending_inputs()[0].target()
    );
}

#[test]
fn creation_admission_collects_all_diagnostics_in_stable_resource_path_code_order() {
    let run = |reverse: bool| {
        let mut rules = vec![
            InputRule::value(
                crate::schema::InputRuleScope::CreateOnly,
                InputPath::new("name").unwrap(),
                InputRuleCode::new("z.length").unwrap(),
                InputPredicate::Length {
                    min: None,
                    max: Some(3),
                },
            ),
            InputRule::value(
                crate::schema::InputRuleScope::CreateOnly,
                InputPath::new("name").unwrap(),
                InputRuleCode::new("a.ipv4").unwrap(),
                InputPredicate::NotIpv4,
            ),
        ];
        if reverse {
            rules.reverse();
        }
        let schema = rules.into_iter().fold(
            crate::schema::ResourceSchema::new("Bucket").attribute(
                AttributeSchema::optional("name", crate::schema::AttributeType::string()).build(),
            ),
            |schema, rule| schema.input_rule(rule),
        );
        let mut schemas = SchemaRegistry::new();
        schemas.insert("mock", schema.validate().unwrap());
        let mut resources =
            [("a", None), ("b", None), ("a", Some("alternate"))].map(|(id, instance)| {
                ResolvedResource::resolve_raw_fixture(
                    Resource::with_provider("mock", "Bucket", id, instance.map(str::to_owned))
                        .with_attribute("name", string("192.168.1.1")),
                    &schemas,
                )
                .unwrap()
            });
        if reverse {
            resources.reverse();
        }
        let mut plan = Plan::new();
        for resource in resources {
            plan.add(Effect::Create(resource));
        }
        PendingCascadePlan {
            registry: schemas.clone(),
            plan,
            deferred: Default::default(),
        }
        .finish(std::iter::empty(), |_| {})
        .check_inputs(&schemas, &HashMap::new())
        .unwrap_err()
        .errors()
        .to_vec()
    };
    let errors = run(false);
    assert_eq!(errors.len(), 6);
    assert_eq!(errors, run(true));
}

#[test]
fn creation_admission_nested_absence_unknown_and_secret_values_remain_distinct() {
    use crate::schema::{AttributeType, StructField};
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        crate::schema::ResourceSchema::new("Bucket")
            .attribute(
                AttributeSchema::optional(
                    "config",
                    AttributeType::struct_(
                        "Config",
                        vec![StructField::required("name", AttributeType::string()).build()],
                    ),
                )
                .build(),
            )
            .input_rule(InputRule::value(
                crate::schema::InputRuleScope::CreateOnly,
                InputPath::new("config.name").unwrap(),
                InputRuleCode::new("bucket.not_ipv4").unwrap(),
                InputPredicate::NotIpv4,
            ))
            .validate()
            .unwrap(),
    );
    let make = |config| {
        let mut resource = Resource::with_provider("mock", "Bucket", "nested", None);
        if let Some(config) = config {
            resource = resource.with_attribute("config", config);
        }
        let mut plan = Plan::new();
        plan.add(Effect::Create(
            ResolvedResource::resolve_raw_fixture(resource, &schemas).unwrap(),
        ));
        PendingCascadePlan {
            registry: schemas.clone(),
            plan,
            deferred: Default::default(),
        }
        .finish(std::iter::empty(), |_| {})
        .check_inputs(&schemas, &HashMap::new())
    };
    assert!(make(None).unwrap().pending_inputs().is_empty());
    assert_eq!(
        make(Some(Value::Concrete(
            ConcreteValue::Map(Default::default())
        )))
        .unwrap()
        .pending_inputs()
        .len(),
        1
    );
    assert_eq!(make(Some(unknown())).unwrap().pending_inputs().len(), 1);
    let secret = string("192.168.1.1").into_secret();
    let rejected = make(Some(Value::Concrete(ConcreteValue::Map(
        indexmap::IndexMap::from([("name".into(), secret)]),
    ))))
    .unwrap_err();
    assert!(
        matches!(&rejected.errors()[0].kind, PlanErrorKind::InputRule { path, .. } if path == "config.name")
    );
    assert!(!format!("{:?}", rejected.errors()).contains("192.168.1.1"));
}

#[test]
fn input_scope_classification_table_covers_every_effect_variant() {
    use crate::schema::{AttributeType, InputRuleScope, ResourceSchema};
    let resource = resource(Some(string("valid-name")));
    let deleted = delete(&resource);
    let attempt = CreateAttempt::new(resource.id.clone()).0;
    let unissued = InFlightCreate::from_pending(attempt.pending(
        PendingToken::new("unissued"),
        Sent::AllRequestAttributes,
        1,
    ));
    let issued = InFlightCreate::from_pending(
        CreateAttempt::new(resource.id.clone())
            .0
            .issued(
                AppliedIdentifier::new("existing").unwrap(),
                Sent::AllRequestAttributes,
            )
            .pending(PendingToken::new("issued"), 1),
    );
    let DeferredCreatePayload {
        template,
        dependency_template,
    } = deferred_payload();
    let mut cases = vec![
        ("create", Effect::Create(resource.clone()), None, 1, 1),
        (
            "resume unissued",
            Effect::ResumeCreate {
                resource: resource.clone(),
                started_at: unissued.started_at().clone(),
            },
            Some(Current::create_in_flight(unissued)),
            1,
            1,
        ),
        (
            "resume issued",
            Effect::ResumeCreate {
                resource: resource.clone(),
                started_at: issued.started_at().clone(),
            },
            Some(Current::create_in_flight(issued)),
            0,
            0,
        ),
        (
            "update",
            Effect::Update {
                from: Box::new(
                    State::existing(resource.id.clone(), HashMap::new()).with_identifier("old"),
                ),
                identifier: AppliedIdentifier::new("old").unwrap(),
                to: resource.clone(),
                changed_attributes: vec!["name".into()],
            },
            None,
            0,
            1,
        ),
        (
            "read",
            Effect::Read {
                resource: crate::resource::ResolvedDataSource::collect_resolved([
                    crate::resource::DataSource::with_provider("mock", "Bucket", "lookup", None),
                ])
                .unwrap()
                .remove(0),
            },
            None,
            0,
            0,
        ),
        (
            "delete",
            Effect::Delete {
                id: deleted.id,
                identifier: deleted.identifier,
                generation: EffectGeneration::Current,
                directives: deleted.directives,
                binding: deleted.binding,
                dependencies: deleted.dependencies,
                explicit_dependencies: deleted.explicit_dependencies,
                blocked_by_updates: Default::default(),
            },
            None,
            0,
            0,
        ),
        (
            "import",
            Effect::Import {
                id: resource.resolved_id().clone(),
                identifier: string("old"),
            },
            None,
            0,
            0,
        ),
        (
            "remove",
            Effect::Remove {
                id: resource.resolved_id().clone(),
            },
            None,
            0,
            0,
        ),
        (
            "move",
            Effect::Move {
                from: resource.resolved_id().clone(),
                to: resource.resolved_id().clone(),
                origin: None,
            },
            None,
            0,
            0,
        ),
        (
            "release",
            Effect::ReleaseListKeys {
                id: resource.resolved_id(),
                attribute: "settings".into(),
                key_field: crate::schema::FieldName::declared("key"),
                keys: crate::effect::ReleasedListKeys::try_new(vec![
                    crate::effect::ReleasedListKey::new("entry", string("value")),
                ])
                .unwrap(),
            },
            None,
            0,
            0,
        ),
        (
            "wait",
            Effect::Wait {
                id: ResolvedResourceId::with_provider_identity("mock", "Bucket", "wait", None),
                target_id: crate::wait::ManagedWaitTarget::new(&resource),
                until: crate::wait::predicate::WaitPredicate::equals(
                    crate::wait::predicate::AttrPath::single("name"),
                    serde_json::from_value::<crate::wait::WaitRhs>(
                        serde_json::json!({"String": "'ready'"}),
                    )
                    .unwrap(),
                )
                .unwrap(),
                until_surface: "resource.name == ready".into(),
                timeout: std::time::Duration::from_secs(1),
                interval: std::time::Duration::from_secs(1),
                explicit_dependencies: Default::default(),
            },
            None,
            0,
            0,
        ),
        (
            "deferred create",
            Effect::DeferredCreate(deferred_payload()),
            None,
            1,
            1,
        ),
        (
            "deferred replace",
            Effect::DeferredReplace(Box::new(DeferredReplacePayload::new(
                NonEmptyDeletes::try_new(vec![DeferredReplaceDelete {
                    generation: EffectGeneration::Current,
                    previous_attributes: HashMap::new(),
                    id: resource.resolved_id().clone(),
                    identifier: AppliedIdentifier::new("old").unwrap(),
                    directives: Default::default(),
                    binding: None,
                    dependencies: Default::default(),
                    explicit_dependencies: Default::default(),
                    blocked_by_updates: Default::default(),
                }])
                .unwrap(),
                template,
                dependency_template,
            ))),
            None,
            1,
            1,
        ),
    ];
    // This match is deliberately exhaustive alongside the explicit table:
    // introducing another Effect variant requires updating its coverage here.
    for (_, effect, _, _, _) in &cases {
        match effect {
            Effect::Read { .. }
            | Effect::Create(_)
            | Effect::ResumeCreate { .. }
            | Effect::Update { .. }
            | Effect::Delete { .. }
            | Effect::Import { .. }
            | Effect::Remove { .. }
            | Effect::Move { .. }
            | Effect::ReleaseListKeys { .. }
            | Effect::Wait { .. }
            | Effect::DeferredCreate(_)
            | Effect::DeferredReplace(_) => {}
        }
    }
    for scope in [InputRuleScope::CreateOnly, InputRuleScope::EveryWrite] {
        let mut schemas = SchemaRegistry::new();
        schemas.insert(
            "mock",
            ResourceSchema::new("Bucket")
                .attribute(AttributeSchema::optional("name", AttributeType::string()).build())
                .input_rule(InputRule::value(
                    scope,
                    InputPath::new("name").unwrap(),
                    InputRuleCode::new("test.scope").unwrap(),
                    InputPredicate::NotIpv4,
                ))
                .validate()
                .unwrap(),
        );
        for (label, effect, prior, create_count, write_count) in &mut cases {
            let mut plan = Plan::new();
            plan.add(effect.clone());
            let current = prior
                .clone()
                .map(|prior| HashMap::from([(resource.id.clone(), prior)]))
                .unwrap_or_default();
            let finalized = PendingCascadePlan {
                registry: schemas.clone(),
                plan,
                deferred: Default::default(),
            }
            .finish(std::iter::empty(), |_| {});
            let count = finalized
                .write_sites(&schemas, &current)
                .map(|site| site.unwrap().applicable_rules().count())
                .sum::<usize>();
            let expected = match scope {
                InputRuleScope::CreateOnly => *create_count,
                InputRuleScope::EveryWrite => *write_count,
            };
            assert_eq!(count, expected, "{label}, {scope:?}");
        }
        for cbd in [false, true] {
            let finalized = PendingCascadePlan {
                registry: schemas.clone(),
                plan: replacement(cbd, resource.clone()),
                deferred: Default::default(),
            }
            .finish(std::iter::empty(), |_| {});
            let current = HashMap::new();
            let sites = finalized
                .write_sites(&schemas, &current)
                .collect::<Result<Vec<_>, _>>()
                .unwrap();
            assert_eq!(sites.len(), 1);
            assert_eq!(
                sites[0].context(),
                InputWriteContext::Replacement {
                    create_before_destroy: cbd
                }
            );
            assert_eq!(sites[0].applicable_rules().count(), 1);
        }
    }
}

#[test]
fn issue_3900_actual_generated_temporary_name_exceeding_length_rejects_plan() {
    use crate::schema::{AttributeType, InputRuleScope, ResourceSchema};
    let original = "a".repeat(24);
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        ResourceSchema::new("Bucket")
            .attribute(
                AttributeSchema::optional("name", AttributeType::string())
                    .create_only()
                    .build(),
            )
            .attribute(
                AttributeSchema::optional("generation", AttributeType::int())
                    .create_only()
                    .build(),
            )
            .with_unique_name_attribute("name")
            .input_rule(InputRule::value(
                InputRuleScope::CreateOnly,
                InputPath::new("name").unwrap(),
                InputRuleCode::new("name.max_length").unwrap(),
                InputPredicate::Length {
                    min: None,
                    max: Some(32),
                },
            ))
            .validate()
            .unwrap(),
    );
    let desired = Resource::with_provider("mock", "Bucket", "bucket", None)
        .with_attribute("name", string(&original))
        .with_attribute("generation", Value::Concrete(ConcreteValue::Int(2)));
    let from = State::existing(
        desired.id.clone(),
        HashMap::from([
            ("name".into(), string(&original).to_snapshot()),
            (
                "generation".into(),
                Value::Concrete(ConcreteValue::Int(1)).to_snapshot(),
            ),
        ]),
    )
    .with_identifier("old");
    let resolved = ResolvedResource::resolve_raw_fixture(desired, &schemas).unwrap();
    let mut old = delete(&resolved);
    old.previous_attributes = from.attributes;
    let mut requested = resolved.as_inner().clone();
    requested.directives.create_before_destroy = true;
    let checked = crate::replacement::resolve_replacement(
        crate::replacement::ReplacementRequest::new(
            resolved.replace_inner(requested),
            NonEmptyDeletes::try_new(vec![old]).unwrap(),
        ),
        &schemas,
    )
    .unwrap();
    let temporary = checked.temporary_name().unwrap().clone();
    assert_eq!(temporary.temporary_value.len(), 33);
    assert!(original.len() <= 32);
    let resolved = checked.target().clone();
    let mut plan = replacement(true, resolved);
    plan.replace_display_mut()[0].temporary_name = Some(temporary.clone());
    let rejected = PendingCascadePlan {
        registry: schemas.clone(),
        plan,
        deferred: Default::default(),
    }
    .finish(std::iter::empty(), |_| {})
    .check_inputs(&schemas, &HashMap::new())
    .unwrap_err();
    assert!(
        matches!(&rejected.errors()[0].kind, PlanErrorKind::InputRule { path, code, .. } if path == "name" && code == "name.max_length")
    );
    assert!(!format!("{:?}", rejected.errors()).contains(&temporary.temporary_value));
}

#[test]
fn creation_admission_does_not_duplicate_existing_replacement_error() {
    let mut registry = SchemaRegistry::new();
    registry.insert(
        "mock",
        crate::schema::ResourceSchema::new("Bucket")
            .with_conflicting_replacement()
            .attribute(
                AttributeSchema::optional("name", crate::schema::AttributeType::string()).build(),
            )
            .validate()
            .unwrap(),
    );
    let mut raw = resource(Some(string("valid-name"))).as_inner().clone();
    raw.directives.create_before_destroy = true;
    let create = ResolvedResource::resolve_with_schemas(raw, &registry).unwrap();
    let mut plan = Plan::new();
    plan.add_delete(delete(&create));
    plan.add(Effect::Create(create));
    let finalized = PendingCascadePlan {
        plan,
        deferred: Default::default(),
        registry: registry.clone(),
    }
    .finish(std::iter::empty(), |_| {});
    assert_eq!(finalized.errors().len(), 1);
    let diagnostic = finalized.errors()[0].clone();
    let rejected = finalized
        .check_inputs(&registry, &HashMap::new())
        .unwrap_err();
    assert_eq!(rejected.errors(), &[diagnostic]);
}

fn issue_4418_authored_pending() -> ReplacementGroup {
    let mut raw = resource(None).into_inner();
    raw.directives.create_before_destroy = true;
    let producer = ResolvedResource::new(raw);
    let old = delete(&producer);
    ReplacementGroup {
        replacement: resolve_replacement(
            ReplacementRequest::new(producer, NonEmptyDeletes::try_new(vec![old]).unwrap()),
            &schemas(),
        )
        .unwrap(),
        changed_create_only: ChangedCreateOnly::new(vec!["name".into()]).unwrap(),
        cascade_ref_hints: Vec::new(),
        consumer_updates: Default::default(),
    }
}

#[test]
fn issue_4418_authored_create_first_promotion_keeps_required_by_absent() {
    let group = issue_4418_authored_pending();
    let id = group.replacement.target().resolved_id();
    let before = serde_json::to_value(group.replacement.clone().into_request()).unwrap();
    assert!(group.replacement.target().directives.create_before_destroy);
    let child = ResolvedResource::from_resolved_parts(
        Resource::with_provider("mock", "Bucket", "consumer", None).with_dependency_phase(()),
        std::iter::once(id.clone()).collect(),
        Default::default(),
    );
    let mut pending = HashMap::from([(id.clone(), group)]);
    let mut plan = Plan::new();
    let dependents = CascadeDependents::new(
        std::slice::from_ref(&child),
        ResolvedDeferredCreateTargets::default(),
    );
    assert!(
        promote_referenced_replaces_to_cbd(&mut plan, &mut pending, &dependents, &schemas())
            .is_continue()
    );
    let after = pending[&id].replacement.clone().into_request();
    assert!(
        after.required_by().is_none(),
        "authored create-first must not acquire a dependent requirement"
    );
    assert_eq!(serde_json::to_value(after).unwrap(), before);
    assert!(plan.errors().is_empty());
}

#[test]
fn issue_4418_authored_create_first_mark_keeps_required_by_absent() {
    let mut group = issue_4418_authored_pending();
    let before = serde_json::to_value(group.replacement.clone().into_request()).unwrap();
    mark_pending_create_before_destroy(
        &mut group,
        &schemas(),
        Dependent::new(ResolvedResourceId::with_provider_identity(
            "mock", "Bucket", "consumer", None,
        ))
        .into(),
    )
    .unwrap();
    let after = group.replacement.into_request();
    assert!(
        after.required_by().is_none(),
        "marking an admitted create-first replacement must be a no-op"
    );
    assert_eq!(serde_json::to_value(after).unwrap(), before);
}
