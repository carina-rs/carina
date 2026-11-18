use super::*;
use crate::effect::Effect;
use crate::resource::{PersistedDestroyDependencies, State};

const EXPECTED: [&str; 6] = ["delta", "gamma", "theta", "zulu", "beta", "alpha"];

fn id(name: &str) -> ResourceId {
    ResourceId::with_provider_identity(
        "mock",
        "test.resource",
        crate::resource::ResourceIdentity::try_from(name.to_owned()).unwrap(),
        None,
    )
}

fn orphans(guarded: bool) -> Orphans {
    ["alpha", "theta", "beta", "delta", "zulu", "gamma"]
        .into_iter()
        .map(|name| {
            let parents = match name {
                "zulu" => vec![id("beta")],
                "beta" => vec![id("alpha")],
                _ => vec![],
            };
            let dependencies = PersistedDestroyDependencies::decode_persisted(
                parents
                    .into_iter()
                    .map(crate::resource::ResolvedResourceId::new)
                    .collect(),
            )
            .unwrap();
            let mut directives = PersistedDirectives::default();
            directives.set_prevent_destroy(guarded);
            // Exercise both persisted dependency sources in the same chain.
            let mut state: State = State::existing(id(name), HashMap::new()).with_identifier(name);
            if name == "beta" {
                directives.depends_on = dependencies;
            } else {
                state.dependency_bindings = dependencies;
            }
            (
                crate::resource::ResolvedResourceId::new(state.id.clone()),
                Orphan::from_state(state, None, directives),
            )
        })
        .collect()
}

fn plan(orphans: &Orphans) -> crate::plan::Plan {
    create_plan_for_tests(
        &[],
        &[],
        &crate::provider::ProviderRouter::new(),
        &HashMap::new(),
        &HashMap::new(),
        &crate::schema::SchemaRegistry::new(),
        &empty_lifted_saved_attrs(),
        &HashMap::new(),
        orphans,
        &[],
    )
}

#[test]
fn issue_4491_orphan_deletes_and_history_use_reverse_dependencies_then_address() {
    for _ in 0..8 {
        let plan = plan(&orphans(false));
        let names: Vec<_> = plan
            .effects()
            .iter()
            .map(|effect| match effect {
                Effect::Delete { id, .. } => id.identity_str(),
                other => panic!("unexpected effect {other:?}"),
            })
            .collect();
        assert_eq!(names, EXPECTED);
        let saved = serde_json::to_value(crate::plan::SavedPlan::from(plan.to_snapshot())).unwrap();
        let history: Vec<_> = saved["replacement_inputs"]["old_instances"]
            .as_array()
            .unwrap()
            .iter()
            .map(|old| old["instance"]["id"]["identity"].as_str().unwrap())
            .collect();
        assert_eq!(history, EXPECTED);
    }
}

#[test]
fn issue_4491_orphan_prevent_destroy_diagnostics_use_delete_order() {
    let plan = plan(&orphans(true));
    let names: Vec<_> = plan
        .errors()
        .iter()
        .map(|error| error.resource_id.identity_str().unwrap())
        .collect();
    assert_eq!(names, EXPECTED);
}

#[test]
fn issue_4491_orphan_order_retains_persisted_dependencies_without_an_applied_snapshot() {
    use crate::provider::{CreateAttempt, InFlightCreate, PendingToken, Sent};
    let mut orphans = orphans(false);
    orphans.remove(&id("zulu"));
    let pending = InFlightCreate::from_pending(CreateAttempt::new(id("zulu")).0.pending(
        PendingToken::new("opaque"),
        Sent::AllRequestAttributes,
        1,
    ));
    let mut child = Orphan::create_in_flight(pending, None, PersistedDirectives::default());
    child.set_destroy_metadata(
        PersistedDestroyDependencies::decode_persisted(
            [crate::resource::ResolvedResourceId::new(id("beta"))]
                .into_iter()
                .collect(),
        )
        .unwrap(),
        PersistedDirectives::default(),
    );
    orphans.insert(crate::resource::ResolvedResourceId::new(id("zulu")), child);
    assert_eq!(
        orphans
            .keys()
            .map(|id| id.identity_str().unwrap())
            .collect::<Vec<_>>(),
        EXPECTED
    );
}

#[test]
fn issue_4491_orphan_cycles_and_missing_dependencies_are_total_and_repeatable() {
    let build = || {
        ["omega", "beta", "alpha", "delta", "gamma"]
            .into_iter()
            .map(|name| {
                let parent = match name {
                    "alpha" => "beta",
                    "beta" => "alpha",
                    _ => "missing",
                };
                let state: State = State::existing(id(name), HashMap::new())
                    .with_identifier(name)
                    .with_dependency_bindings(
                        PersistedDestroyDependencies::decode_persisted(
                            [crate::resource::ResolvedResourceId::new(id(parent))]
                                .into_iter()
                                .collect(),
                        )
                        .unwrap(),
                    );
                (
                    crate::resource::ResolvedResourceId::new(state.id.clone()),
                    Orphan::from_state(state, None, PersistedDirectives::default()),
                )
            })
            .collect::<Orphans>()
    };
    assert_eq!(
        build()
            .keys()
            .map(|id| id.identity_str().unwrap().to_owned())
            .collect::<Vec<_>>(),
        ["alpha", "beta", "delta", "gamma", "omega"]
    );
    let first =
        serde_json::to_vec(&crate::plan::SavedPlan::from(plan(&build()).to_snapshot())).unwrap();
    for _ in 0..16 {
        assert_eq!(
            serde_json::to_vec(&crate::plan::SavedPlan::from(plan(&build()).to_snapshot()))
                .unwrap(),
            first
        );
    }
}

#[test]
fn issue_4491_orphan_cycle_keeps_acyclic_dependency_after_cycle_members() {
    for names in [["aaa", "b", "c"], ["c", "aaa", "b"], ["b", "c", "aaa"]] {
        let orphans: Orphans = names
            .into_iter()
            .map(|name| {
                let parents: &[&str] = match name {
                    "b" => &["aaa", "c"],
                    "c" => &["b"],
                    _ => &[],
                };
                let state: State = State::existing(id(name), HashMap::new())
                    .with_identifier(name)
                    .with_dependency_bindings(
                        PersistedDestroyDependencies::decode_persisted(
                            parents
                                .iter()
                                .map(|name| crate::resource::ResolvedResourceId::new(id(name)))
                                .collect(),
                        )
                        .unwrap(),
                    );
                (
                    crate::resource::ResolvedResourceId::new(state.id.clone()),
                    Orphan::from_state(state, None, PersistedDirectives::default()),
                )
            })
            .collect();
        assert_eq!(
            plan(&orphans)
                .effects()
                .iter()
                .map(|effect| effect.resource_id().identity_str().unwrap().to_owned())
                .collect::<Vec<_>>(),
            ["b", "c", "aaa"]
        );
    }
}

#[test]
fn issue_4491_orphan_ready_components_break_ties_by_smallest_address() {
    for names in [["a", "b", "z", "y"], ["y", "z", "b", "a"]] {
        let orphans: Orphans = names
            .into_iter()
            .map(|name| {
                let parent = match name {
                    "a" => Some("b"),
                    "b" => Some("a"),
                    "z" => Some("y"),
                    _ => None,
                };
                let dependencies = PersistedDestroyDependencies::decode_persisted(
                    parent
                        .map(|name| crate::resource::ResolvedResourceId::new(id(name)))
                        .into_iter()
                        .collect(),
                )
                .unwrap();
                let state: State = State::existing(id(name), HashMap::new())
                    .with_identifier(name)
                    .with_dependency_bindings(dependencies);
                (
                    crate::resource::ResolvedResourceId::new(state.id.clone()),
                    Orphan::from_state(state, None, PersistedDirectives::default()),
                )
            })
            .collect();
        assert_eq!(
            orphans
                .keys()
                .map(|id| id.identity_str().unwrap())
                .collect::<Vec<_>>(),
            ["a", "b", "z", "y"]
        );
        assert_eq!(
            plan(&orphans)
                .effects()
                .iter()
                .map(|effect| effect.resource_id().identity_str().unwrap().to_owned())
                .collect::<Vec<_>>(),
            ["a", "b", "z", "y"]
        );
    }
}

#[test]
fn issue_4491_orphan_transformed_dependencies_drive_order_and_delete_edges() {
    let parent: State = State::existing(id("aaa"), HashMap::new()).with_identifier("aaa");
    let child: State = State::existing(id("zzz"), HashMap::new()).with_identifier("zzz");
    let mut orphans = Orphans::from(
        [
            Orphan::from_state(parent, None, Default::default()),
            Orphan::from_state(child.clone(), None, Default::default()),
        ]
        .map(|orphan| {
            (
                crate::resource::ResolvedResourceId::new(orphan.current().id().clone()),
                orphan,
            )
        }),
    );
    // Populate the cached order before the provider transform changes edges.
    assert_eq!(orphans.keys().next(), Some(&id("aaa")));
    let dependencies = PersistedDestroyDependencies::decode_persisted(
        [crate::resource::ResolvedResourceId::new(id("aaa"))]
            .into_iter()
            .collect(),
    )
    .unwrap();
    let transformed = child
        .with_dependency_bindings(dependencies.clone())
        .to_snapshot();
    orphans.apply_transformed_states(&mut HashMap::from([(id("zzz"), transformed)]));
    let orphan = orphans.get(&id("zzz")).unwrap();
    let Current::Present(present) = orphan.current() else {
        panic!("present orphan")
    };
    assert!(std::ptr::eq(
        orphan.destroy_dependencies(),
        &present.state().dependency_bindings
    ));
    let plan = plan(&orphans);
    assert_eq!(
        plan.effects()
            .iter()
            .map(|effect| effect.resource_id().identity_str().unwrap())
            .collect::<Vec<_>>(),
        ["zzz", "aaa"]
    );
    let Effect::Delete {
        dependencies: actual,
        ..
    } = &plan.effects()[0]
    else {
        panic!("delete")
    };
    assert_eq!(actual, &dependencies);
}

#[test]
fn issue_4491_stateless_orphan_dependencies_survive_current_round_trip() {
    use crate::provider::{CreateAttempt, InFlightCreate, PendingToken, Sent};
    let pending = InFlightCreate::from_pending(CreateAttempt::new(id("pending")).0.pending(
        PendingToken::new("opaque"),
        Sent::AllRequestAttributes,
        1,
    ));
    let failure = CreateAttempt::new(id("unknown")).0.outcome_unknown(
        crate::provider::EgressSent::for_test(),
        crate::provider::ProviderError::api_error("guest trapped"),
    );
    let crate::provider::CreateFailureKind::OutcomeUnknown { unknown, .. } = failure.into_kind()
    else {
        panic!("unknown outcome");
    };
    let dependencies = PersistedDestroyDependencies::decode_persisted(
        [crate::resource::ResolvedResourceId::new(id("parent"))]
            .into_iter()
            .collect(),
    )
    .unwrap();
    for mut orphan in [
        Orphan::create_in_flight(pending, None, PersistedDirectives::default()),
        Orphan::create_outcome_unknown(unknown, None, PersistedDirectives::default()),
    ] {
        orphan.set_destroy_metadata(dependencies.clone(), PersistedDirectives::default());
        let restored = match orphan.into_current() {
            Current::CreateInFlight(pending) => {
                Orphan::create_in_flight(pending, None, PersistedDirectives::default())
            }
            Current::CreateOutcomeUnknown(unknown) => {
                Orphan::create_outcome_unknown(unknown, None, PersistedDirectives::default())
            }
            other => panic!("expected stateless orphan: {other:?}"),
        };
        assert_eq!(restored.destroy_dependencies(), &dependencies);
    }
}

#[test]
fn issue_4491_moved_orphan_to_undeclared_address_uses_destination_provider_route() {
    use crate::effect::ResolvedMove;
    use crate::parser::MovedAddresses;
    use crate::resource::ResolvedResourceId;

    let from = ResolvedResourceId::with_provider_identity(
        "mock",
        "test.resource",
        "old_name",
        Some("source".to_string()),
    );
    let to = ResolvedResourceId::with_provider_identity(
        "mock",
        "test.resource",
        "new_name",
        Some("destination".to_string()),
    );
    let resolved_move = ResolvedMove::from_moved_block(
        &MovedAddresses::new("mock", "test.resource", "old_name", "new_name"),
        from.clone(),
        to.clone(),
    )
    .expect("a moved block may transfer state between provider routes");
    let authority = resolved_move.authority();
    let state: State =
        State::existing(from.as_inner().clone(), HashMap::new()).with_identifier("object-123");
    let mut directives = PersistedDirectives::default();
    directives.provider_instance = Some("source".to_string());
    let mut orphans = Orphans::from([(from, Orphan::from_state(state, None, directives))]);

    // With no desired resources, move materialization retains this as an orphan.
    assert!(orphans.rekey(&authority));
    assert!(orphans.get(authority.from()).is_none());
    let orphan = orphans
        .get(authority.to())
        .expect("the orphan lookup key must carry the destination provider route");
    assert_eq!(orphan.current().id(), authority.to());
    assert!(matches!(
        plan(&orphans).effects(),
        [Effect::Delete { id, .. }]
            if id == &to && id.provider_instance() == Some("destination")
    ));
}

#[test]
fn issue_4491_resolved_orphan_addresses_have_total_order() {
    use crate::resource::ResolvedResourceId;
    let first = ResolvedResourceId::with_provider_identity("mock", "a.b", "c", None);
    let second = ResolvedResourceId::with_provider_identity("mock", "a", "b.c", None);
    assert_eq!(
        first.to_string(),
        second.to_string(),
        "exercise the final structural tie break"
    );
    assert_ne!(first, second);
    let order = |ids: [ResolvedResourceId; 2]| {
        CanonicalOrphanRows::new(ids.map(|id| (id, Default::default(), ())))
            .keys()
            .cloned()
            .collect::<Vec<_>>()
    };
    let expected = vec![second.clone(), first.clone()];
    assert_eq!(order([first.clone(), second.clone()]), expected);
    assert_eq!(order([second, first]), expected);
}
