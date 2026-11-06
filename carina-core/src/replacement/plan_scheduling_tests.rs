use std::collections::{BTreeSet, HashMap};

use crate::effect::{
    DeferredCreatePayload, DeferredReplaceDelete, DeferredReplacePayload, Effect, EffectGeneration,
    NonEmptyDeletes,
};
use crate::executor::{
    PlanSchedulerEndpoint, PlanSchedulerPhase, build_plan_scheduler_dependency_edges,
};
use crate::parser::{DeferredForExpression, DeferredForTarget, ForBinding};
use crate::plan::{CreateBeforeDestroyError, Plan};
use crate::provider::{AppliedIdentifier, CreateAttempt, PendingToken, Sent};
use crate::resource::{
    ConcreteValue, Current, DataSource, DeferredDependencyTemplate, DeposedKey, LoopSlot,
    ResolvedDataSource, ResolvedResource, ResolvedResourceId, Resource, RootScope, Value,
};
use crate::schema::{PatternResourceSchema, SchemaRegistry};

fn old(id: ResolvedResourceId, generation: EffectGeneration) -> DeferredReplaceDelete {
    DeferredReplaceDelete {
        id,
        generation,
        identifier: AppliedIdentifier::new("old-id").unwrap(),
        directives: Default::default(),
        binding: None,
        dependencies: Default::default(),
        explicit_dependencies: Default::default(),
        blocked_by_updates: Default::default(),
        previous_attributes: Default::default(),
    }
}

fn schemas(coexisting: bool) -> SchemaRegistry {
    let mut schemas = SchemaRegistry::new();
    for kind in ["Parent", "Child"] {
        let schema = PatternResourceSchema::new(kind);
        schemas.insert(
            "mock",
            if coexisting {
                schema.with_coexisting_replacement()
            } else {
                schema.with_conflicting_replacement()
            }
            .validate()
            .unwrap(),
        );
    }
    schemas
}

#[test]
fn deferred_alias_requirement_is_rejected_before_materialization() {
    let target = ResolvedResourceId::with_provider_identity(
        "mock",
        "Child",
        "children[0]",
        Some("new".into()),
    );
    for absorbed in [false, true] {
        let old = old(
            ResolvedResourceId::with_provider_identity(
                "mock",
                "Child",
                "children[0]",
                Some("old".into()),
            ),
            EffectGeneration::Current,
        );
        let template = DeferredForExpression::new(
            None,
            1,
            "for value in []".into(),
            DeferredForTarget::bound(RootScope::new().resolve("children".into())).unwrap(),
            Value::Concrete(ConcreteValue::List(Vec::new())),
            ForBinding::Simple("value".into()),
            Resource::<_, LoopSlot>::with_provider("mock", "Child", "children", Some("new".into())),
        )
        .into_compiled();
        let mut plan = Plan::new();
        if absorbed {
            plan.add(Effect::DeferredReplace(Box::new(
                DeferredReplacePayload::new(
                    NonEmptyDeletes::try_new(vec![old]).unwrap(),
                    Box::new(template),
                    DeferredDependencyTemplate::default(),
                ),
            )));
        } else {
            plan.add_delete(old);
            plan.add(Effect::DeferredCreate(DeferredCreatePayload::new(
                Box::new(template),
                DeferredDependencyTemplate::default(),
            )));
        }
        plan.add(Effect::Create(ResolvedResource::from_resolved_parts(
            Resource::with_provider("mock", "Consumer", "consumer", None).with_dependency_phase(()),
            std::iter::once(target.clone()).collect(),
            Default::default(),
        )));
        for saved in [false, true] {
            let admission = if saved {
                let wire: crate::plan::UnresolvedSavedPlan =
                    serde_json::from_value(serde_json::to_value(plan.to_snapshot()).unwrap())
                        .unwrap();
                wire.resolve_with_schemas(&schemas(false))
                    .unwrap()
                    .admit(&schemas(false))
                    .map(|_| ())
            } else {
                plan.clone().admit(&schemas(false)).map(|_| ())
            };
            let error = admission.expect_err("the projected target alias requires an impossible create-first Child before execution");
            assert!(matches!(
                error.cause,
                CreateBeforeDestroyError::ReplacementCannotCoexist(_)
            ));
            assert!(error.required_by.is_some());
        }
    }
}

fn read_gate_plan(bound: bool, generation: EffectGeneration) -> (Plan, ResolvedResourceId) {
    let parent = ResolvedResourceId::with_provider_identity("mock", "Parent", "parent", None);
    let lookup = ResolvedResourceId::with_provider_identity("mock", "Lookup", "lookup", None);
    let parsed = crate::parser::parse_file(
        "let upstream = upstream_state { source = '../upstream' }\nfor value in upstream.values { mock.Child {} }\n",
        std::path::Path::new("main.crn"),
        &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    ).unwrap();
    let parsed = crate::validation::deferred_types::with_session_for_test(
        &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        |session| {
            crate::module_resolver::resolve_modules_providerless(
                crate::config_loader::ParsedRootDirectory::from_parsed_for_test(parsed),
                &crate::paths::ResolvedPath::new(std::path::Path::new(".")).unwrap(),
                session,
            )
            .map_err(Box::new)
        },
    )
    .unwrap()
    .into_expanded(
        &SchemaRegistry::new(),
        &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    )
    .unwrap();
    let template = if bound {
        DeferredForExpression::new(
            None,
            1,
            "for value in lookup.values".into(),
            DeferredForTarget::bound(RootScope::new().resolve("children".into())).unwrap(),
            Value::root_resource_ref("lookup", "values", Vec::new()),
            ForBinding::Simple("value".into()),
            Resource::<_, LoopSlot>::with_provider("mock", "Child", "children", None),
        )
        .into_compiled()
    } else {
        parsed.deferred_for_expressions()[0].clone().into_compiled()
    };
    let mut plan = Plan::new();
    plan.add(Effect::Create(ResolvedResource::from_id(parent.clone())));
    plan.add_delete(old(parent.clone(), EffectGeneration::Current));
    let mut child = old(
        ResolvedResourceId::with_provider_identity(
            "mock",
            "Child",
            if bound {
                "children[0]"
            } else {
                "anonymous-child"
            },
            None,
        ),
        generation,
    );
    child.dependencies = crate::test_support::persisted([parent.clone()]);
    child.explicit_dependencies = std::iter::once(parent.clone()).collect();
    plan.add_delete(child);
    plan.add(Effect::Read {
        resource: ResolvedDataSource::from_resolved_parts(
            DataSource::with_provider("mock", "Lookup", "lookup", None).with_dependency_phase(()),
            std::iter::once(parent.clone()).collect(),
            Default::default(),
        ),
    });
    plan.add(Effect::DeferredCreate(DeferredCreatePayload::new(
        Box::new(template),
        DeferredDependencyTemplate::from_graph(
            std::iter::once(lookup).collect(),
            Default::default(),
            Default::default(),
            Default::default(),
            Default::default(),
        ),
    )));
    (plan, parent)
}

#[test]
fn unbound_gate_read_barrier_promotes_parent_before_execution() {
    let (plan, parent) = read_gate_plan(false, EffectGeneration::Current);
    let admitted = plan.admit(&schemas(true)).unwrap();
    let (expanded, _, _) = admitted.admitted_effects().expand_deferred_replacements();
    let graph = expanded.dependency_analysis(std::iter::empty()).unwrap();
    assert!(
        graph.deps_of(1).unwrap().contains(&0),
        "the parent must be create-first before any execution barrier is installed"
    );
    assert!(!graph.deps_of(0).unwrap().contains(&1));
    assert!(
        graph.deps_of(2).unwrap().contains(&4),
        "admission's graph must contain the anonymous Delete's gate barrier"
    );
    assert_eq!(expanded.effects()[0].resource_id(), parent.as_inner());
}

#[test]
fn unbound_gate_read_barrier_rejects_conflicting_parent_before_execution() {
    let (plan, _) = read_gate_plan(false, EffectGeneration::Current);
    let error = plan
        .admit(&schemas(false))
        .expect_err("a Conflicting parent cannot break the gate/Read replacement cycle");
    assert!(matches!(
        error.cause,
        CreateBeforeDestroyError::ReplacementCannotCoexist(_)
    ));
}

#[test]
fn gate_read_barriers_survive_known_child_projection_and_saved_plan_loading() {
    for bound in [false, true] {
        for generation in [
            EffectGeneration::Current,
            EffectGeneration::Deposed(DeposedKey::new_unique()),
        ] {
            let (plan, _) = read_gate_plan(bound, generation);
            let wire: crate::plan::UnresolvedSavedPlan =
                serde_json::from_value(serde_json::to_value(plan.to_snapshot()).unwrap()).unwrap();
            let admitted = wire
                .resolve_with_schemas(&schemas(true))
                .unwrap()
                .admit(&schemas(true))
                .unwrap();
            let scheduled = admitted.admitted_effects();
            let graph = scheduled.dependency_analysis(std::iter::empty()).unwrap();
            assert!(
                graph.deps_of(1).unwrap().contains(&0),
                "the parent must be promoted even when the child's Create was projected before admission"
            );
            assert!(graph.deps_of(2).unwrap().contains(&4));
            let mut completed = std::collections::HashSet::new();
            loop {
                let before = completed.len();
                for index in 0..scheduled.effects().len() {
                    if graph.deps_of(index).unwrap().is_subset(&completed) {
                        completed.insert(index);
                    }
                }
                if completed.len() == before {
                    break;
                }
            }
            assert_eq!(
                completed.len(),
                scheduled.effects().len(),
                "admission cannot leave a replacement barrier cycle for execution"
            );
        }
    }
}

#[test]
fn deferred_alias_requirement_preserves_sibling_orders_after_reload() {
    let id = |identity: &'static str, route: &str| {
        ResolvedResourceId::with_provider_identity("mock", "Child", identity, Some(route.into()))
    };
    let target = id("children[0]", "new");
    let template = DeferredForExpression::new(
        None,
        1,
        "for value in []".into(),
        DeferredForTarget::bound(RootScope::new().resolve("children".into())).unwrap(),
        Value::Concrete(ConcreteValue::List(Vec::new())),
        ForBinding::Simple("value".into()),
        Resource::<_, LoopSlot>::with_provider("mock", "Child", "children", Some("new".into())),
    )
    .into_compiled();
    let mut plan = Plan::new();
    plan.add(Effect::DeferredReplace(Box::new(
        DeferredReplacePayload::new(
            NonEmptyDeletes::try_new(vec![
                old(id("children[0]", "old-current"), EffectGeneration::Current),
                old(
                    id("children[0]", "old-deposed"),
                    EffectGeneration::Deposed(DeposedKey::new_unique()),
                ),
                old(id("children[1]", "old-current"), EffectGeneration::Current),
            ])
            .unwrap(),
            Box::new(template),
            Default::default(),
        ),
    )));
    plan.add(Effect::Create(ResolvedResource::from_resolved_parts(
        Resource::with_provider("mock", "Consumer", "consumer", None).with_dependency_phase(()),
        std::iter::once(target).collect(),
        Default::default(),
    )));
    let wire: crate::plan::UnresolvedSavedPlan =
        serde_json::from_value(serde_json::to_value(plan.to_snapshot()).unwrap()).unwrap();
    let admitted = wire
        .resolve_with_schemas(&schemas(true))
        .unwrap()
        .admit(&schemas(true))
        .unwrap();
    let Effect::DeferredReplace(payload) = &admitted.effects()[0] else {
        panic!("the deferred gate must survive reload");
    };
    let requests = payload.preflight_requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].deletes.len(), 2);
    assert!(requests[0].required_by().is_some());
    assert!(requests[1].required_by().is_none());
    for request in &requests {
        assert_eq!(
            request.target().id.provider_instance.as_deref(),
            Some("new")
        );
        assert!(!request.target().directives.create_before_destroy);
    }
    assert_eq!(
        requests[0].deletes[0].id.provider_instance.as_deref(),
        Some("old-current")
    );
    assert_eq!(
        requests[0].deletes[1].id.provider_instance.as_deref(),
        Some("old-deposed")
    );
}

fn relaxed_update_plan() -> Plan {
    use crate::resource::{ResolvedDependencyReads, SnapshotState};

    let id =
        |kind, identity| ResolvedResourceId::with_provider_identity("mock", kind, identity, None);
    let parent = id("Parent", "parent");
    let first = id("Updater", "first");
    let second = id("Updater", "second");
    let lookup = id("Lookup", "lookup");
    let update = |target: ResolvedResourceId, dependency: ResolvedResourceId, attribute: &str| {
        let mut reads = ResolvedDependencyReads::default();
        reads.insert_known(dependency.clone(), attribute);
        Effect::Update {
            from: Box::new(SnapshotState::existing(
                target.as_inner().clone(),
                Default::default(),
            )),
            identifier: AppliedIdentifier::new("existing-id").unwrap(),
            to: ResolvedResource::from_resolved_parts_with_reads(
                Resource::from_id(target.into_inner()).with_dependency_phase(()),
                std::iter::once(dependency).collect(),
                Default::default(),
                reads,
            ),
            changed_attributes: vec!["changed".into()],
        }
    };
    let mut previous = old(parent.clone(), EffectGeneration::Current);
    previous.blocked_by_updates = std::iter::once(first.clone()).collect();
    let mut plan = Plan::new();
    plan.add_delete(previous);
    plan.add(Effect::Create(ResolvedResource::from_id(parent.clone())));
    plan.add(update(first, second.clone(), "unchanged"));
    plan.add(update(second, lookup, "changed"));
    plan.add(Effect::Read {
        resource: ResolvedDataSource::from_resolved_parts(
            DataSource::with_provider("mock", "Lookup", "lookup", None).with_dependency_phase(()),
            std::iter::once(parent).collect(),
            Default::default(),
        ),
    });
    plan
}

#[test]
fn admission_uses_the_scheduler_relaxation_before_promoting() {
    let admitted = relaxed_update_plan()
        .admit(&schemas(false))
        .expect("a disjoint Update edge must not demand a Conflicting create-first replacement");
    let graph = admitted
        .admitted_effects()
        .dependency_analysis(std::iter::empty())
        .unwrap();
    assert!(graph.deps_of(1).unwrap().contains(&0));
    assert!(!graph.deps_of(2).unwrap().contains(&3));
}

#[test]
fn scheduled_plan_retains_its_admitted_source() {
    let (mut plan, _) = read_gate_plan(true, EffectGeneration::Current);
    let child = plan
        .effects_mut()
        .remove(2)
        .to_delete_payload(|_: &crate::effect::DeletedInstanceKey| Default::default())
        .unwrap();
    let Effect::DeferredCreate(gate) = plan.effects_mut().pop().unwrap() else {
        panic!("fixture must end with its deferred gate");
    };
    plan.add(Effect::DeferredReplace(Box::new(
        DeferredReplacePayload::new(
            NonEmptyDeletes::try_new(vec![child]).unwrap(),
            gate.template,
            gate.dependency_template,
        ),
    )));
    let schemas = schemas(true);
    let admitted = plan.admit(&schemas).unwrap();
    let scheduled = admitted.schedule(&Default::default());
    assert!(std::ptr::eq(scheduled.plan(), &admitted));

    let (expanded, teardowns, _) = scheduled.clone().expand_deferred_replacements();
    assert!(
        !teardowns.is_empty(),
        "fixture must expand an absorbed teardown"
    );
    assert!(std::ptr::eq(expanded.plan(), &admitted));
    let extra = ResolvedResource::from_id(ResolvedResourceId::with_provider_identity(
        "mock", "Parent", "extra", None,
    ));
    let materialized = crate::replacement::AdmittedReplacementEffects::with_materialized(
        &expanded,
        vec![Effect::Create(extra)],
        Vec::new(),
        &schemas,
    )
    .unwrap();
    let extended = expanded.with_materialized(materialized);
    assert!(std::ptr::eq(extended.plan(), &admitted));
    assert_eq!(extended.effects().len(), scheduled.effects().len() + 2);
}

#[test]
fn identified_continuation_diagram_matches_scheduled_cleanup() {
    for (identified, available) in [(true, true), (false, true), (false, false)] {
        let id = ResolvedResourceId::with_provider_identity("mock", "Parent", "parent", None);
        let (attempt, _cancellation) = CreateAttempt::new(id.as_inner().clone());
        let pending = if identified {
            attempt
                .issued(
                    AppliedIdentifier::new("new-id").unwrap(),
                    Sent::AllRequestAttributes,
                )
                .pending(PendingToken::new("poll"), 1)
        } else {
            attempt.pending(PendingToken::new("resend"), Sent::AllRequestAttributes, 1)
        }
        .persist(Default::default(), Default::default());
        let mut plan = Plan::new();
        plan.add_resume_create(
            ResolvedResource::from_id(id.clone()),
            pending.started_at().clone(),
        );
        plan.add_delete(old(
            id.clone(),
            EffectGeneration::Deposed(DeposedKey::new_unique()),
        ));
        let admitted = plan.admit(&schemas(false)).unwrap();
        let current = if available {
            HashMap::from([(id.into_inner(), Current::CreateInFlight(pending))])
        } else {
            HashMap::new()
        };
        let scheduled = admitted.schedule(&current);
        let graph = scheduled.dependency_analysis(std::iter::empty()).unwrap();
        let primary = |effect_index| PlanSchedulerEndpoint {
            effect_index,
            phase: PlanSchedulerPhase::Primary,
        };
        let (before, after) = if identified || !available {
            (0, 1)
        } else {
            (1, 0)
        };
        assert!(graph.deps_of(after).unwrap().contains(&before));
        assert!(!graph.deps_of(before).unwrap().contains(&after));
        let diagram = build_plan_scheduler_dependency_edges(&scheduled, &HashMap::new());
        assert_eq!(
            diagram,
            BTreeSet::from([(primary(before), primary(after))]),
            "the diagram must reuse the continuation-specialized schedule"
        );
    }
}
